#![cfg(not(target_arch = "wasm32"))]

use std::fs;
use std::sync::{Arc, Mutex, OnceLock};

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::tasks::futures_lite::future;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use ffxi_dat::mmb::{parse_models, MmbHeader};
use ffxi_dat::texture::{decode_texture, DecodedTexture};
use ffxi_dat::{mmb, walk, ChunkKind, DatRoot};

use crate::dat_mzb::diag_rss_mb;
use crate::ffxi_zone_material::FfxiZoneMaterial;
use crate::graphics_settings::GraphicsSettings;
use crate::look_resolver::dispatch_look_driven_models;
use crate::scene::TrackedEntities;
use crate::zone_texture::{decoded_texture_to_image, TextureQuality};

#[derive(Component)]
pub struct MmbOverlay;

/// The original [`LoadMmbRequest`] that spawned a zone placement, stamped on the
/// placement's parent so the distance-retirement pass can re-queue it (the
/// request is `Copy` — this is the ~100B per placement cost of restartable
/// streaming). Entity-attached models are excluded: they are few and small and
/// follow their tracked entity instead of the streaming radius.
#[derive(Component, Debug, Clone, Copy)]
pub struct ZonePlacementRef(pub LoadMmbRequest);

#[derive(Resource, Default)]
pub struct MmbHandleCache {
    pub mesh: std::collections::HashMap<(u32, usize, usize), bevy::asset::Handle<Mesh>>,
    /// Keyed by (file_id, chunk_idx, sub_index, mirrored). The mirror bit is
    /// part of the pipeline key (front-face flip for negative-determinant
    /// placements — xim GLDrawer.kt:186), so the same submesh placed both
    /// ways needs two material instances.
    pub material:
        std::collections::HashMap<(u32, usize, usize, bool), bevy::asset::Handle<FfxiZoneMaterial>>,
}

#[derive(Resource, Default)]
pub struct MmbLoadQueue {
    pub pending: std::collections::VecDeque<LoadMmbRequest>,

    pub last_eval_pos: Option<Vec3>,

    pub budget_deferred: bool,
}

// Below one yalm of movement the distance-sorted streaming order cannot
// meaningfully change; re-sorting the retained far queue every frame anyway
// measured 0.47ms steady-state in the 100-PC Jeuno capture (2026-07-31).
const MMB_REEVAL_MOVE_YALMS: f32 = 1.0;

fn mmb_repass_needed(
    new_events: bool,
    parse_completed: bool,
    budget_deferred: bool,
    settings_changed: bool,
    last_eval_pos: Option<Vec3>,
    self_pos: Option<Vec3>,
) -> bool {
    if new_events || parse_completed || budget_deferred || settings_changed {
        return true;
    }
    match (last_eval_pos, self_pos) {
        (Some(prev), Some(now)) => {
            prev.distance_squared(now) > MMB_REEVAL_MOVE_YALMS * MMB_REEVAL_MOVE_YALMS
        }
        (None, None) => false,
        _ => true,
    }
}

#[derive(Resource, Default)]
pub struct MmbParseCache {
    pub by_asset: std::collections::HashMap<(u32, usize), Option<LoadedMmb>>,
}

#[derive(Resource, Default)]
pub struct MmbLoadInFlight {
    pub tasks: std::collections::HashMap<(u32, usize), Task<Option<LoadedMmb>>>,
}

#[derive(Resource, Default)]
pub struct MmbTexPools {
    pub by_file: std::collections::HashMap<
        u32,
        (
            std::collections::HashMap<String, Handle<Image>>,
            Option<Handle<Image>>,
        ),
    >,
}

/// Last texture-filtering anisotropy applied to the pooled images, so the
/// live-apply system can skip redundant GPU re-uploads when an unrelated
/// graphics setting changes.
#[derive(Resource, Default)]
pub struct AppliedTextureFiltering {
    pub anisotropy: Option<u16>,
}

impl MmbParseCache {
    /// Drop parse state for one chunk, once every placement of it has retired.
    pub fn drop_chunk(&mut self, file_id: u32, chunk_idx: usize) {
        self.by_asset.remove(&(file_id, chunk_idx));
    }
}

impl MmbHandleCache {
    /// Drop mesh/material handles for one chunk (matched like the keys).
    /// Releases the strong clones that pin the Bevy assets; `track_assets`
    /// frees them once the entities referencing them despawn too.
    pub fn drop_chunk(&mut self, file_id: u32, chunk_idx: usize) {
        self.mesh
            .retain(|(f, c, _), _| *f != file_id || *c != chunk_idx);
        self.material
            .retain(|(f, c, _, _), _| *f != file_id || *c != chunk_idx);
    }
}

impl MmbTexPools {
    /// Drop pooled texture handles for a whole DAT file.
    pub fn drop_file(&mut self, file_id: u32) {
        self.by_file.remove(&file_id);
    }
}

// A generator-driven water sheet (ffxi-dat Generator::parse_model_spawn): the
// broad canal/harbor water (e.g. Port Windurst tshimonosea1/2) is instanced by a
// zone Generator, not the MZB object list, and carries its own translucent tint
// (alpha < 1) and per-layer UV-scroll velocity. When a LoadMmbRequest carries
// this, the spawned submeshes render blended with the tint and scroll their UVs.
#[derive(Debug, Clone, Copy)]
pub struct GenWater {
    pub tint: Vec4,
    pub uv_scroll: Vec2,
    /// World-space AABB of the transformed sheet (MMB header bounds × the
    /// generator transform), used to suppress the placeholder MZB water plane
    /// where the retail sheet already covers the surface.
    pub world_min: Vec3,
    pub world_max: Vec3,
}

#[derive(Message, Debug, Clone, Copy)]
pub struct LoadMmbRequest {
    pub file_id: u32,
    pub chunk_idx: usize,

    pub world_pos: Vec3,
    pub entity_id: Option<u32>,

    pub world_transform: Option<Mat4>,

    pub water: Option<GenWater>,

    pub lod: Option<crate::dat_mzb::ZoneMeshLod>,

    pub door: Option<crate::zone_doors::ZoneDoorLeaf>,

    /// Zone block this placement belongs to, stamped on the spawned entity as
    /// [`crate::dat_mzb::ZoneBlockSlot`] so retiring an interior finds it.
    /// Ignored for entity-attached models, which no zone block owns.
    pub slot: u8,

    /// `MmbPlacement::sub_area_link`; 0 for everything that is not the exterior
    /// shell of an interior.
    pub sub_area_link: u32,
}

// Animates one generator water sheet's UV scroll. Each sheet owns its material
// (never the shared cache), so the two stacked Port Windurst layers scroll at
// their own velocities. uv_offset flows to the GPU through FfxiZoneMaterial's
// persistent instance buffer; get_mut_untracked avoids rebuilding the bind group
// every frame (same pattern as scroll_water_uv / zone_clouds).
#[derive(Component)]
pub struct GenWaterScroll {
    pub material: Handle<FfxiZoneMaterial>,
    pub uv_scroll: Vec2,
}

pub fn scroll_gen_water_uv(
    time: Res<Time>,
    q: Query<&GenWaterScroll>,
    mut materials: ResMut<Assets<FfxiZoneMaterial>>,
) {
    // The generator's 0x27/0x28 velocity is UV per retail frame, not per second
    // (research/xim TextureCoordinateUpdater; same convention as zone_clouds
    // drift_clouds) — raw seconds scrolls the sea 30× too slow to perceive.
    let t = time.elapsed_secs() * crate::scheduler_runtime::RETAIL_FPS;
    for gw in q.iter() {
        if let Some(mat) = materials.get_mut_untracked(&gw.material) {
            // fract keeps the offset small for f32 precision over long sessions;
            // the texture repeats so a whole-tile jump is invisible.
            let u = (gw.uv_scroll.x * t).fract();
            let v = (gw.uv_scroll.y * t).fract();
            mat.uv_offset = Vec4::new(u, v, 0.0, 0.0);
        }
    }
}

pub struct DatOverlayPlugin;

impl Plugin for DatOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(crate::ffxi_actor_render::despawn_morph_column);
        app.add_message::<LoadMmbRequest>()
            .add_message::<crate::dat_vos2::LoadVos2Request>()
            .add_message::<crate::ffxi_actor_render::LoadActorRequest>()
            .add_message::<crate::dat_mzb::LoadMzbRequest>()
            .add_message::<crate::sub_area_activation::SubAreaChanged>()
            .add_message::<crate::sub_area_activation::SetSubArea>()
            .init_resource::<crate::sub_area_activation::SubAreaActivation>()
            .init_resource::<MmbHandleCache>()
            .init_resource::<MmbLoadQueue>()
            .init_resource::<MmbParseCache>()
            .init_resource::<MmbLoadInFlight>()
            .init_resource::<MmbTexPools>()
            .init_resource::<AppliedTextureFiltering>()
            .init_resource::<ZoneBakeState>()
            .init_resource::<crate::dat_mzb::LastAutoLoadedZone>()
            .init_resource::<crate::dat_mzb::DrawDistance>()
            .init_resource::<crate::dat_mzb::StreamingAnchor>()
            .init_resource::<crate::dat_mzb::MzbCollisionGeometry>()
            .init_resource::<crate::dat_mzb::ZoneAreaMap>()
            .init_resource::<crate::dat_mzb::ZoneChunkLightMap>()
            .init_resource::<crate::dat_mzb::LoadMzbInFlight>()
            .init_resource::<crate::dat_mzb::ZoneGeomCache>()
            .init_resource::<crate::dat_mzb::PendingWaterSpawns>()
            .init_resource::<crate::dat_mzb::ZoneWaterMaterial>()
            .init_resource::<crate::ffxi_actor_render::ActorLoadInFlight>()
            .add_systems(
                Update,
                (
                    crate::dat_mzb::auto_load_zone_geometry_system,
                    crate::sub_area_activation::drive_sub_area_activation,
                    dispatch_look_driven_models,
                    crate::dat_mzb::kick_load_mzb_tasks,
                    crate::dat_mzb::poll_load_mzb_tasks,
                    crate::dat_mzb::spawn_zone_water,
                    process_load_mmb_requests,
                    poll_zone_bake,
                    retire_far_zone_placements,
                    crate::ffxi_actor_render::kick_load_actor_tasks,
                    crate::ffxi_actor_render::poll_load_actor_tasks,
                    crate::ffxi_actor_render::tick_morph_in,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    crate::dat_mzb::cull_entities_by_distance,
                    crate::dat_mzb::select_zone_mmb_lod,
                    crate::dat_mzb::apply_sub_area_shell_visibility,
                    crate::dat_mzb::apply_zone_geom_visibility,
                    crate::dat_mzb::scroll_water_uv,
                    scroll_gen_water_uv,
                ),
            )
            .add_systems(
                Update,
                apply_texture_filtering_system.run_if(resource_changed::<GraphicsSettings>),
            );
    }
}

pub struct MmbSubMesh {
    pub variant_name: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,

    pub blending: u16,
}

#[derive(Debug, Clone)]
pub struct NamedTexture {
    pub name: String,
    pub texture: DecodedTexture,
}

pub struct LoadedMmb {
    pub submeshes: Vec<MmbSubMesh>,
    pub textures: Arc<Vec<NamedTexture>>,

    pub asset_name: String,

    /// Header bytes 16..32 (XIM's section `name`). A leading '_' selects the
    /// alpha-tested cutout render mode for this model's submeshes.
    pub zone_mesh_name: String,
}

/// Decoded-once-per-file MMB texture sets, shared by every chunk parse of the
/// same DAT. An MMB chunk parse (`load_mmb`) walks the whole file's `Img`
/// section, so without sharing a resident radius of K chunks holds K copies of
/// the same texture set — the dominant term in zone-geometry RSS (a city-block
/// file decodes ~120MB, so 80 resident chunks pinned ~9.5GB even though the
/// geometry itself was a few MB). The first parse of a file pays the decode;
/// the rest clone the `Arc`. Dropped when the last chunk of the file retires
/// or the zone changes.
static MMB_FILE_TEXTURES: OnceLock<Mutex<std::collections::HashMap<u32, Arc<Vec<NamedTexture>>>>> =
    OnceLock::new();

fn mmb_file_textures_bank() -> &'static Mutex<std::collections::HashMap<u32, Arc<Vec<NamedTexture>>>>
{
    MMB_FILE_TEXTURES.get_or_init(<_>::default)
}

/// Drop a whole file's shared texture set (last chunk retired / zone changed).
pub fn drop_mmb_file_textures(file_id: u32) {
    mmb_file_textures_bank().lock().unwrap().remove(&file_id);
}

/// Drop every file's shared texture set (zone changed).
pub fn clear_mmb_file_textures() {
    mmb_file_textures_bank().lock().unwrap().clear();
}

pub fn load_mmb(file_id: u32, chunk_idx: usize) -> Result<LoadedMmb, String> {
    let root =
        DatRoot::from_env_or_default().map_err(|e| format!("DatRoot::from_env_or_default: {e}"))?;
    let location = root
        .resolve(file_id)
        .map_err(|e| format!("resolve({file_id}): {e}"))?;
    let path = location.path_under(&root);
    let bytes = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;

    let chunks: Vec<_> = walk(&bytes).filter_map(Result::ok).collect();
    let chunk = chunks.get(chunk_idx).ok_or_else(|| {
        format!(
            "file has {} chunks, idx {chunk_idx} out of range",
            chunks.len()
        )
    })?;
    if ChunkKind::from_u8(chunk.kind) != Some(ChunkKind::Mmb) {
        return Err(format!(
            "chunk {chunk_idx} kind={:#x} ({:?}), not an MMB",
            chunk.kind,
            ChunkKind::label(chunk.kind),
        ));
    }

    let decrypted = mmb::decrypt(chunk.data).map_err(|e| format!("decrypt: {e}"))?;
    let header = MmbHeader::parse(&decrypted).map_err(|e| format!("header parse: {e}"))?;

    let models = parse_models(&decrypted);

    let textures: Arc<Vec<NamedTexture>> = {
        let mut bank = mmb_file_textures_bank().lock().unwrap();
        bank.entry(file_id)
            .or_insert_with(|| {
                let set: Vec<NamedTexture> = chunks
                    .iter()
                    .filter(|c| ChunkKind::from_u8(c.kind) == Some(ChunkKind::Img))
                    .filter_map(|c| {
                        let texture = decode_texture(c.data).ok()?;
                        let name =
                            ffxi_dat::texture::extract_texture_name(c.data).unwrap_or_default();
                        Some(NamedTexture { name, texture })
                    })
                    .collect();
                Arc::new(set)
            })
            .clone()
    };

    let mut out = Vec::with_capacity(models.len());
    for m in &models {
        if m.vertices.is_empty() || m.indices.is_empty() {
            continue;
        }

        const COORD_SANE_LIMIT: f32 = 10_000.0;
        if m.vertices.iter().any(|v| {
            v.pos
                .iter()
                .any(|c| !c.is_finite() || c.abs() > COORD_SANE_LIMIT)
        }) {
            continue;
        }
        let positions: Vec<[f32; 3]> = m.vertices.iter().map(|v| v.pos).collect();
        let normals: Vec<[f32; 3]> = m.vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = m.vertices.iter().map(|v| v.uv).collect();

        let colors: Vec<[f32; 4]> = m
            .vertices
            .iter()
            .map(|v| mmb::vertex_color_to_linear(v.rgba))
            .collect();

        let vert_count = m.vertices.len() as u16;
        let indices: Vec<u32> = m
            .indices
            .chunks_exact(3)
            .filter(|t| t[0] < vert_count && t[1] < vert_count && t[2] < vert_count)
            .flat_map(|t| [t[0] as u32, t[1] as u32, t[2] as u32])
            .collect();
        if indices.is_empty() {
            continue;
        }
        out.push(MmbSubMesh {
            variant_name: m.texture_name.clone(),
            positions,
            normals,
            uvs,
            colors,
            indices,
            blending: m.blending,
        });
    }

    let asset_name = header.asset_name_str().trim().to_string();
    // XIM (`ZoneMeshSection.kt`): the model name at header bytes 16..32 is the
    // alpha-test selector — a leading '_' marks a cutout (foliage) model.
    let zone_mesh_name = header.zone_mesh_name();
    Ok(LoadedMmb {
        submeshes: out,
        textures,
        asset_name,
        zone_mesh_name,
    })
}

fn is_zone_placement(req: &LoadMmbRequest) -> bool {
    req.entity_id.is_none() && req.world_transform.is_some()
}

fn mmb_dist_sq_xz(req: &LoadMmbRequest, self_pos: Vec3) -> f32 {
    let p = req
        .world_transform
        .map(|m| m.w_axis.truncate())
        .unwrap_or(req.world_pos);
    let dx = p.x - self_pos.x;
    let dz = p.z - self_pos.z;
    dx * dx + dz * dz
}

fn mmb_load_order_key(req: &LoadMmbRequest, self_pos: Vec3) -> f32 {
    if is_zone_placement(req) {
        mmb_dist_sq_xz(req, self_pos)
    } else {
        -1.0
    }
}

pub fn process_load_mmb_requests(
    mut events: MessageReader<LoadMmbRequest>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FfxiZoneMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut toasts: MessageWriter<crate::snapshot::ToastEvent>,
    tracked: Res<TrackedEntities>,
    mut handle_cache: ResMut<MmbHandleCache>,
    mut queue: ResMut<MmbLoadQueue>,
    mut parse_cache: ResMut<MmbParseCache>,
    mut tex_pools_res: ResMut<MmbTexPools>,
    settings: Res<GraphicsSettings>,
    self_q: Query<&GlobalTransform, With<crate::components::IsSelf>>,
    stream_anchor: Res<crate::dat_mzb::StreamingAnchor>,
    mut in_flight: ResMut<MmbLoadInFlight>,
) {
    let mut newly_parsed: Vec<((u32, usize), Option<LoadedMmb>)> = Vec::new();
    in_flight.tasks.retain(
        |asset, task| match future::block_on(future::poll_once(task)) {
            Some(result) => {
                newly_parsed.push((*asset, result));
                false
            }
            None => true,
        },
    );
    let parse_completed = !newly_parsed.is_empty();
    for (asset, result) in newly_parsed {
        parse_cache.by_asset.entry(asset).or_insert(result);
    }
    if std::env::var("FFXI_DIAG_STREAM").is_ok() && parse_completed {
        let mut sum = 0usize;
        for l in parse_cache.by_asset.values().flatten() {
            for s in &l.submeshes {
                sum += s.positions.len() * 12
                    + s.normals.len() * 12
                    + s.uvs.len() * 8
                    + s.indices.len() * 4;
            }
        }
        // CPU-side decoded raster lives once per resident file (the shared bank).
        // Count resident files and the chunk-parses that reference each one, to
        // prove decoded memory is bounded by FILE count, not chunk count.
        let (unique_tex, bank_files, refs_per_file) = {
            let bank = mmb_file_textures_bank().lock().unwrap();
            let mut refs: std::collections::HashMap<u32, usize> = Default::default();
            for (file_id, _) in parse_cache.by_asset.keys() {
                if bank.contains_key(file_id) {
                    *refs.entry(*file_id).or_insert(0) += 1;
                }
            }
            (
                bank.values()
                    .map(|v| v.iter().map(|t| t.texture.rgba.len()).sum::<usize>())
                    .sum::<usize>(),
                bank.len(),
                refs,
            )
        };
        // GPU-side: the pooled Assets<Image> bytes per resident file (one upload
        // per file, shared by every placement). Distinguishes the intended 2x
        // hold (CPU decode + GPU Image) from any per-chunk duplication.
        let mut gpu_by_file: Vec<(u32, usize, usize)> = Vec::new();
        for (fid, (by_name, _)) in &tex_pools_res.by_file {
            let mut gpu_bytes = 0usize;
            for h in by_name.values() {
                if let Some(img) = images.get(h) {
                    gpu_bytes += img.data.as_ref().map(|d| d.len()).unwrap_or(0);
                }
            }
            gpu_by_file.push((*fid, gpu_bytes, by_name.len()));
        }
        gpu_by_file.sort_by_key(|(fid, _, _)| *fid);
        let gpu_total: usize = gpu_by_file.iter().map(|(_, b, _)| b).sum();
        let bank_guard = mmb_file_textures_bank().lock().unwrap();
        let mut per_file: Vec<String> = gpu_by_file
            .iter()
            .map(|(fid, g, n)| {
                let cpu = bank_guard
                    .get(fid)
                    .map(|set| set.iter().map(|t| t.texture.rgba.len()).sum::<usize>())
                    .unwrap_or(0);
                let chunk_refs = refs_per_file.get(fid).copied().unwrap_or(0);
                format!(
                    "fid {fid}: cpu {:.1}MB gpu {:.1}MB tex {n} chunks {chunk_refs}",
                    cpu as f64 / 1_048_576.0,
                    *g as f64 / 1_048_576.0
                )
            })
            .collect();
        drop(bank_guard);
        per_file.sort();
        info!(
            "DIAG mmb cache: chunks {} geo MB {:.1} cpu_tex MB {:.1} gpu_tex MB {:.1} (bank files {}; gpu files {}) rss_mb {} per [{}]",
            parse_cache.by_asset.len(),
            sum as f64 / 1_048_576.0,
            unique_tex as f64 / 1_048_576.0,
            gpu_total as f64 / 1_048_576.0,
            bank_files,
            gpu_by_file.len(),
            diag_rss_mb(),
            per_file.join(" | "),
        );
    }

    let pending_before = queue.pending.len();
    queue.pending.extend(events.read().copied());
    let new_events = queue.pending.len() != pending_before;
    if queue.pending.is_empty() {
        return;
    }

    let self_pos = self_q
        .single()
        .ok()
        .map(|t| t.translation())
        .or(stream_anchor.0);
    if std::env::var("FFXI_DIAG_STREAM").is_ok()
        && !queue
            .last_eval_pos
            .is_some_and(|l| self_pos.is_some_and(|s| l.distance_squared(s) < 1.0))
    {
        info!(
            "DIAG mmb pass: self_pos {:?} pending {} rss_mb {}",
            self_pos,
            queue.pending.len(),
            diag_rss_mb()
        );
    }
    if !mmb_repass_needed(
        new_events,
        parse_completed,
        queue.budget_deferred,
        settings.is_changed(),
        queue.last_eval_pos,
        self_pos,
    ) {
        return;
    }
    queue.last_eval_pos = self_pos;
    queue.budget_deferred = false;

    if let Some(self_pos) = self_pos {
        queue.pending.make_contiguous().sort_by(|a, b| {
            mmb_load_order_key(a, self_pos).total_cmp(&mmb_load_order_key(b, self_pos))
        });
    }
    let load_radius = settings.view_distance * crate::dat_mzb::MMB_LOAD_DISTANCE_MARGIN;
    let load_radius_sq = load_radius * load_radius;

    let mut mmb_logged: std::collections::HashSet<(u32, usize)> = std::collections::HashSet::new();

    let diag_file_id: Option<u32> = match std::env::var("FFXI_DIAG_ZONE_GEOM") {
        Ok(s) if s == "*" || s == "all" || s.eq_ignore_ascii_case("any") => Some(u32::MAX),
        Ok(s) => s.parse::<u32>().ok(),
        _ => None,
    };
    let mut diag_zero_submesh: std::collections::HashMap<u32, Vec<(usize, String)>> =
        std::collections::HashMap::new();
    let mut diag_loaded: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut diag_load_failed: std::collections::HashMap<u32, u32> =
        std::collections::HashMap::new();
    let diag_matches = |fid: u32| -> bool {
        match diag_file_id {
            Some(u32::MAX) => true,
            Some(want) => want == fid,
            None => false,
        }
    };

    // Time-bounded spawn pacing, not a fixed count: pouring 96 placements into
    // one frame pinned frames to ~6fps while the backdrop flight drained a
    // ~19k-placement zone (live evidence: ~55s of startup stutter, kuluu-wisp-x1l).
    // Leave ~2.6ms of a 16.6ms frame for the rest of Update + render so streaming
    // can't starve frame pacing; the count ceiling still bounds a frame where
    // placements are pathological-cheap.
    //
    // The two consumers get different budgets. The launcher backdrop flight has
    // no `IsSelf` (self_pos falls back to `stream_anchor`), and its camera pans
    // over unloadable countryside — smooth pacing is the whole point there, so it
    // keeps the cinematic 14ms default. A real in-game session DOES have `IsSelf`,
    // and the player needs the ground under their feet now. Spawn is bounded by
    // the budget but breaks at the first far placement, so cost tracks how many
    // near placements are actually pending: a login into a fresh zone finds
    // hundreds pending and bursts, while the handful that enter the radius on a
    // boundary crossing settles in a few milliseconds regardless.
    //
    // Streaming a login neighborhood at the backdrop pace (~2 placements/frame,
    // ~67/s) left the zone floor visibly missing for 30-60s with repeated
    // multi-second rgraph stalls as the deferred geometry finally compiled (live
    // login evidence, 2026-09-07, zone 67/file 167). The session budget drains
    // the same neighborhood in a burst of a few frames (~5fps for ~1s).
    let in_real_session = self_q.single().is_ok();
    let frame_budget_ms: u64 = if in_real_session {
        std::env::var("FFXI_MMB_SESSION_BUDGET_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(160)
    } else {
        std::env::var("FFXI_MMB_BUDGET_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(14)
    };
    const MMB_SPAWN_FRAME_CEILING: usize = 1024;
    const HEAVY: usize = 8;
    const MMB_MAX_INFLIGHT: usize = 64;
    let spawn_start = std::time::Instant::now();
    let mut spawned = 0usize;
    let mut placed = 0usize;
    let mut place_ns_total = 0u128;
    let mut pool_ns = 0u128;
    let mut mesh_ns = 0u128;
    let mut retained: std::collections::VecDeque<LoadMmbRequest> =
        std::collections::VecDeque::with_capacity(queue.pending.len());

    while let Some(req) = queue.pending.pop_front() {
        if let Some(self_pos) = self_pos {
            if is_zone_placement(&req) && mmb_dist_sq_xz(&req, self_pos) > load_radius_sq {
                retained.push_back(req);
                retained.append(&mut queue.pending);
                break;
            }
        }

        let asset = (req.file_id, req.chunk_idx);
        match parse_cache.by_asset.get(&asset) {
            Some(Some(loaded)) => {
                if diag_matches(req.file_id) {
                    *diag_loaded.entry(req.file_id).or_insert(0) += 1;
                }

                if loaded.submeshes.is_empty() {
                    if diag_matches(req.file_id) {
                        diag_zero_submesh
                            .entry(req.file_id)
                            .or_default()
                            .push((req.chunk_idx, loaded.asset_name.clone()));
                    }

                    if req.world_transform.is_none() {
                        push_system_msg(
                            &mut toasts,
                            format!(
                                "/load_mmb {} {}: 0 renderable sub-records",
                                req.file_id, req.chunk_idx,
                            ),
                        );
                    }
                    continue;
                }

                let pool_exists = tex_pools_res.by_file.contains_key(&req.file_id);
                let cost = if pool_exists { 1 } else { HEAVY };
                if spawned > 0
                    && (spawn_start.elapsed().as_millis() as u64 >= frame_budget_ms
                        || spawned + cost > MMB_SPAWN_FRAME_CEILING)
                {
                    queue.budget_deferred = true;
                    retained.push_back(req);
                    retained.append(&mut queue.pending);
                    break;
                }
                spawned += cost;

                let plc_t0 = std::time::Instant::now();
                let quality = TextureQuality {
                    mipmaps: settings.texture_filtering.mipmaps(),
                    anisotropy: settings.texture_filtering.anisotropy(),
                };
                let pool_t0 = std::time::Instant::now();
                build_texture_pool(
                    &mut images,
                    &mut tex_pools_res.by_file,
                    req.file_id,
                    &loaded.textures,
                    quality,
                );
                let pool = tex_pools_res
                    .by_file
                    .get(&req.file_id)
                    .expect("pool built above");
                let tex_by_name = &pool.0;
                let first_texture = pool.1.clone();
                pool_ns += pool_t0.elapsed().as_nanos();

                if mmb_logged.insert((req.file_id, req.chunk_idx)) {
                    let mut img_stats: Vec<(String, u8, u8)> = loaded
                        .textures
                        .iter()
                        .filter(|nt| !nt.name.is_empty())
                        .map(|nt| {
                            let (mut amin, mut amax) = (255u8, 0u8);
                            for px in nt.texture.rgba.chunks_exact(4) {
                                amin = amin.min(px[3]);
                                amax = amax.max(px[3]);
                            }
                            (nt.name.clone(), amin, amax)
                        })
                        .collect();
                    img_stats.sort_by(|a, b| a.0.cmp(&b.0));
                    let img_names: Vec<String> = img_stats
                        .into_iter()
                        .map(|(n, amin, amax)| format!("{n} α[{amin}..{amax}]"))
                        .collect();
                    let mut requested: Vec<&str> = loaded
                        .submeshes
                        .iter()
                        .map(|s| s.variant_name.as_str())
                        .collect();
                    requested.sort_unstable();
                    requested.dedup();
                    let (matched, unmatched): (Vec<&str>, Vec<&str>) = requested
                        .iter()
                        .partition(|n| tex_by_name.contains_key(**n));

                    let mut blending_view: Vec<(String, u16)> = loaded
                        .submeshes
                        .iter()
                        .map(|s| (s.variant_name.clone(), s.blending))
                        .collect();
                    blending_view.sort_by(|a, b| a.0.cmp(&b.0));
                    let blending_strs: Vec<String> = blending_view
                        .into_iter()
                        .map(|(name, b)| format!("{name}:0x{b:04X}"))
                        .collect();
                    debug!(
                        target: "kuluu_render::dat_mmb",
                        file_id = req.file_id,
                        chunk_idx = req.chunk_idx,
                        asset = %loaded.asset_name,
                        mesh_name = %loaded.zone_mesh_name,
                        cutout = loaded.zone_mesh_name.starts_with('_'),
                        submesh_count = loaded.submeshes.len(),
                        img_count = tex_by_name.len(),
                        imgs = ?img_names,
                        matched = ?matched,
                        unmatched = ?unmatched,
                        blending = ?blending_strs,
                        first_fallback = first_texture.is_some(),
                        "MMB texture pool",
                    );
                }

                let is_static_placement = req
                    .entity_id
                    .and_then(|id| tracked.by_id.get(&id))
                    .is_none();
                let parent = match req.entity_id.and_then(|id| tracked.by_id.get(&id)) {
                    Some(&bevy_e) => {
                        commands.entity(bevy_e).remove::<Mesh3d>();
                        bevy_e
                    }
                    None => {
                        if let Some(missing) = req.entity_id {
                            push_system_msg(
                                &mut toasts,
                                format!(
                            "/load_mmb_on {missing} {} {}: no tracked entity for id {missing} \
                             — spawning at world_pos instead",
                            req.file_id, req.chunk_idx,
                        ),
                            );
                        }
                        let parent_transform = match req.world_transform {
                            Some(m) => Transform::from_matrix(m),
                            None => Transform::from_translation(req.world_pos),
                        };
                        let is_zone_spawn =
                            req.entity_id.is_none() && req.world_transform.is_some();

                        let mut e = commands.spawn((
                            MmbOverlay,
                            crate::components::InGameEntity,
                            parent_transform,
                            Visibility::default(),
                        ));
                        if is_zone_spawn {
                            e.insert((
                                crate::dat_mzb::AutoMzbOverlay,
                                crate::dat_mzb::ZoneBlockSlot(req.slot),
                                ZonePlacementRef(req),
                            ));
                            if req.sub_area_link != 0 {
                                e.insert(crate::dat_mzb::ZoneSubAreaLink(req.sub_area_link));
                            }
                        }
                        if let Some(lod) = req.lod {
                            e.insert(lod);
                        }
                        if let Some(door) = req.door {
                            e.insert(door);
                        }
                        e.id()
                    }
                };

                let n_subs = loaded.submeshes.len();
                let mut plc_bytes = 0usize;
                let mesh_t0 = std::time::Instant::now();
                for (sub_index, sub) in loaded.submeshes.iter().enumerate() {
                    let cache_key = (req.file_id, req.chunk_idx, sub_index);
                    let is_new_mesh = !handle_cache.mesh.contains_key(&cache_key);
                    if is_new_mesh {
                        plc_bytes += sub.positions.len() * 12
                            + sub.normals.len() * 12
                            + sub.uvs.len() * 8
                            + sub.colors.len() * 4
                            + sub.indices.len() * 4;
                    }

                    let mesh_handle = handle_cache
                        .mesh
                        .entry(cache_key)
                        .or_insert_with(|| {
                            let mut mesh = Mesh::new(
                                PrimitiveTopology::TriangleList,
                                RenderAssetUsages::default(),
                            );
                            mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, sub.positions.clone());
                            mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, sub.normals.clone());
                            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, sub.uvs.clone());
                            mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, sub.colors.clone());
                            mesh.insert_indices(Indices::U32(sub.indices.clone()));
                            meshes.add(mesh)
                        })
                        .clone();

                    let variant_trimmed = sub.variant_name.trim();
                    let sub_texture = tex_by_name
                        .get(variant_trimmed)
                        .cloned()
                        .or_else(|| first_texture.clone());

                    let (alpha_mode, discard_threshold) = submesh_alpha_mode(
                        &loaded.zone_mesh_name,
                        sub.blending,
                        sub_texture.is_some(),
                    );
                    let has_texture = if sub_texture.is_some() { 1.0 } else { 0.0 };
                    let blend_flag = if matches!(alpha_mode, AlphaMode::Blend) {
                        1.0
                    } else {
                        0.0
                    };

                    // Render-state word -> pipeline key (cull / z-bias / depth
                    // write). See FfxiZoneMaterialKey and ffxi-dat MmbRenderState.
                    //
                    // FFXI winding is clockwise (D3D). Mirrored placements
                    // (negative-determinant transforms, ubiquitous for zone
                    // tiles) flip effective winding, so the front-face choice
                    // must ride the pipeline key per placement — xim
                    // GLDrawer.kt:186 does the same via glFrontFace.
                    let rs = ffxi_dat::mmb::MmbRenderState::from_blending(sub.blending);
                    let mirrored = req.world_transform.is_some_and(|m| m.determinant() < 0.0);
                    let render_key = crate::ffxi_zone_material::FfxiZoneMaterialKey {
                        back_face_culling: rs.back_face_culling,
                        mirrored,
                        z_bias_level: rs.z_bias_level(),
                        depth_write: rs.depth_write(),
                        // Placed zone geometry is drawn by the ZoneRenderer, not off a
                        // generator, so it takes the single-MODULATE2X terrain chain.
                        generator_stage_chain: false,
                    };

                    // Generator water (sea sheets): force the translucent blend
                    // path and apply the generator tint (alpha < 1). Each water
                    // sheet gets its OWN material (never the shared cache) so its
                    // per-layer UV-scroll animates independently.
                    let mat_handle = if let Some(w) = req.water {
                        materials.add(FfxiZoneMaterial::new(
                            sub_texture,
                            crate::skinned_ffxi_material::FfxiMaterialFlags {
                                flags: Vec4::new(
                                    has_texture,
                                    1.0,
                                    crate::ffxi_zone_material::ZONE_FLAG_FOGGED,
                                    0.0,
                                ),
                            },
                            w.tint,
                            Vec4::ZERO,
                            AlphaMode::Blend,
                            crate::ffxi_zone_material::FfxiZoneMaterialKey {
                                back_face_culling: false,
                                mirrored,
                                // A full water sheet is not a coplanar decal:
                                // decal z-bias would pull it toward the camera and
                                // let it float over terrain that should occlude it.
                                z_bias_level: 0,
                                depth_write: false,
                                // A sea sheet hangs off a water generator, so it takes the
                                // two-stage CMoD3m chain with `w.tint` as its TEXTUREFACTOR.
                                generator_stage_chain: true,
                            },
                        ))
                    } else {
                        handle_cache
                            .material
                            .entry((cache_key.0, cache_key.1, cache_key.2, mirrored))
                            .or_insert_with(|| {
                                materials.add(FfxiZoneMaterial::new(
                                    sub_texture,
                                    crate::skinned_ffxi_material::FfxiMaterialFlags {
                                        flags: Vec4::new(
                                            has_texture,
                                            blend_flag,
                                            crate::ffxi_zone_material::ZONE_FLAG_FOGGED,
                                            discard_threshold,
                                        ),
                                    },
                                    Vec4::ONE,
                                    Vec4::ZERO,
                                    alpha_mode,
                                    render_key,
                                ))
                            })
                            .clone()
                    };

                    let mut child = commands.spawn((
                        MmbOverlay,
                        Mesh3d(mesh_handle),
                        MeshMaterial3d(mat_handle.clone()),
                        Transform::default(),
                        ChildOf(parent),
                    ));

                    if let Some(w) = req.water {
                        child.insert((
                            GenWaterScroll {
                                material: mat_handle,
                                uv_scroll: Vec2::from_array([w.uv_scroll.x, w.uv_scroll.y]),
                            },
                            bevy::light::NotShadowCaster,
                            bevy::light::NotShadowReceiver,
                        ));
                    }

                    if is_static_placement {
                        child.insert((
                            crate::components::CameraOccluder,
                            // Static world geometry also renders to the minimap's
                            // offscreen top-down bake camera (RenderLayers::layer(
                            // MINIMAP_BAKE_LAYER)); actors/sky/weather stay on the
                            // default layer only, so the bake captures the zone.
                            bevy::camera::visibility::RenderLayers::default()
                                .with(crate::minimap::topdown::MINIMAP_BAKE_LAYER),
                        ));
                    }

                    child.insert(mesh_debug_bundle(crate::components::MmbDebugInfo {
                        file_id: req.file_id,
                        chunk_idx: req.chunk_idx,
                        sub_index,
                        asset_name: loaded.asset_name.clone(),
                        variant_name: sub.variant_name.trim().to_string(),
                    }));
                }
                mesh_ns += mesh_t0.elapsed().as_nanos();

                let is_zone_spawn = req.entity_id.is_none() && req.world_transform.is_some();
                if std::env::var("FFXI_DIAG_PLC").is_ok() && is_zone_spawn {
                    info!(
                        "DIAG mmb plc: fid {} chunk {} subs {} new_bytes MB {:.1} rss_mb {} name {}",
                        req.file_id,
                        req.chunk_idx,
                        n_subs,
                        plc_bytes as f64 / 1_048_576.0,
                        diag_rss_mb(),
                        loaded.zone_mesh_name
                    );
                }
                if !is_zone_spawn {
                    let where_ = match req.entity_id {
                        Some(id) => format!("on entity {id}"),
                        None => format!(
                            "at ({:.1}, {:.1}, {:.1})",
                            req.world_pos.x, req.world_pos.y, req.world_pos.z,
                        ),
                    };
                    let tex_note = match tex_by_name.len() {
                        0 => " (no texture)".to_string(),
                        1 => " +1 texture".to_string(),
                        n => format!(" +{n} textures"),
                    };
                    push_system_msg(
                        &mut toasts,
                        format!(
                            "/load_mmb {} {}: spawned {n_subs} sub-mesh{} {where_}{tex_note}",
                            req.file_id,
                            req.chunk_idx,
                            if n_subs == 1 { "" } else { "es" },
                        ),
                    );
                }
                placed += 1;
                place_ns_total += plc_t0.elapsed().as_nanos();
            }
            Some(None) => {
                push_system_msg(
                    &mut toasts,
                    format!("/load_mmb {} {}: load failed", req.file_id, req.chunk_idx),
                );
                if diag_matches(req.file_id) {
                    *diag_load_failed.entry(req.file_id).or_insert(0) += 1;
                }
            }
            None => {
                if !in_flight.tasks.contains_key(&asset) && in_flight.tasks.len() < MMB_MAX_INFLIGHT
                {
                    let pool = AsyncComputeTaskPool::get();
                    let (file_id, chunk_idx) = (req.file_id, req.chunk_idx);
                    in_flight.tasks.insert(
                        asset,
                        pool.spawn(async move { load_mmb(file_id, chunk_idx).ok() }),
                    );
                }
                retained.push_back(req);
            }
        }
    }
    queue.pending = retained;

    if std::env::var("FFXI_DIAG_STREAM").is_ok() {
        info!(
            "DIAG mmb end: spawned {spawned} this pass, placed {placed} avg {:.2}ms (pool {:.2} mesh {:.2} rest {:.2}) spent {:.1}ms budget, pending retained {} rss_mb {}",
            place_ns_total as f64 / 1_000_000.0 / placed.max(1) as f64,
            pool_ns as f64 / 1_000_000.0 / placed.max(1) as f64,
            mesh_ns as f64 / 1_000_000.0 / placed.max(1) as f64,
            (place_ns_total.saturating_sub(pool_ns + mesh_ns)) as f64 / 1_000_000.0 / placed.max(1) as f64,
            spawn_start.elapsed().as_secs_f64() * 1000.0,
            queue.pending.len(),
            diag_rss_mb()
        );
    }

    if diag_file_id.is_some() {
        for (fid, examples) in &diag_zero_submesh {
            if examples.is_empty() {
                continue;
            }
            let loaded = diag_loaded.get(fid).copied().unwrap_or(0);
            let load_failed = diag_load_failed.get(fid).copied().unwrap_or(0);
            let head: Vec<&(usize, String)> = examples.iter().take(20).collect();
            info!(
                target: "kuluu_render::dat_mmb::diag",
                file_id = *fid,
                loaded,
                load_failed,
                zero_submesh = examples.len(),
                "DIAG-zonegeom zero-submesh MMBs (chunk_idx, asset_name, top 20): {head:?}",
            );
        }

        for (fid, loaded) in &diag_loaded {
            if diag_zero_submesh
                .get(fid)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                continue;
            }
            let load_failed = diag_load_failed.get(fid).copied().unwrap_or(0);
            info!(
                target: "kuluu_render::dat_mmb::diag",
                file_id = *fid,
                loaded = *loaded,
                load_failed,
                zero_submesh = 0,
                "DIAG-zonegeom MMB pass: all submeshes non-empty",
            );
        }
    }
}

/// How far past the spawn radius a placement must be before it is retired,
/// so a placement spawned at the radius edge is not despawned by trivial
/// boundary jitter; the extra margin is the hysteresis gap between
/// `process_load_mmb_requests` (spawn at `load_radius`) and retirement.
const RETIRE_MARGIN: f32 = 1.5;

/// Whether a main-slot zone placement at `pos` is beyond the retirement radius
/// for a streaming center at `center`: `RETIRE_MARGIN * load_radius`, measured
/// on the XZ plane like the spawn ordering (`mmb_dist_sq_xz`).
fn zone_placement_should_retire(center: Vec3, pos: Vec3, load_radius: f32) -> bool {
    let retire_r2 = (load_radius * RETIRE_MARGIN).powi(2);
    let d2 = (pos.x - center.x).powi(2) + (pos.z - center.z).powi(2);
    d2 > retire_r2
}

/// Retire zone-model placements that have fallen far outside the streaming
/// radius, so resident geometry stays bounded to the band around the player /
/// backdrop flight camera instead of accumulating across a whole zone visit.
///
/// Despawned parents re-queue their original [`LoadMmbRequest`] on the pending
/// stream (sorted far-last, re-spawned when the center approaches again). Once
/// a `(file_id, chunk_idx)` has no surviving placement, its parse, mesh and
/// material caches are dropped too — releasing the decoded submesh/texture
/// arrays immediately and the Bevy Mesh/Image/Material once `track_assets`
/// observes the handle count reaching zero. The camera's movement gates the
/// scan (same `MMB_REEVAL_MOVE_YALMS` deadband as the spawn repass); a static
/// center changes nothing worth re-evaluating.
pub fn retire_far_zone_placements(
    mut commands: Commands,
    settings: Res<GraphicsSettings>,
    self_q: Query<&GlobalTransform, With<crate::components::IsSelf>>,
    stream_anchor: Res<crate::dat_mzb::StreamingAnchor>,
    mut queue: ResMut<MmbLoadQueue>,
    mut parse_cache: ResMut<MmbParseCache>,
    mut handle_cache: ResMut<MmbHandleCache>,
    mut tex_pools: ResMut<MmbTexPools>,
    mut in_flight: ResMut<MmbLoadInFlight>,
    mut last_center: Local<Option<Vec3>>,
    placements_q: Query<
        (
            Entity,
            &GlobalTransform,
            &ZonePlacementRef,
            &crate::dat_mzb::ZoneBlockSlot,
        ),
        With<crate::dat_mzb::AutoMzbOverlay>,
    >,
) {
    let Some(center) = self_q
        .single()
        .ok()
        .map(|t| t.translation())
        .or(stream_anchor.0)
    else {
        return;
    };
    let deadband = MMB_REEVAL_MOVE_YALMS * MMB_REEVAL_MOVE_YALMS;
    if last_center
        .is_some_and(|prev| prev.distance_squared(center) <= deadband && !settings.is_changed())
    {
        return;
    }
    *last_center = Some(center);

    let load_radius = settings.view_distance * crate::dat_mzb::MMB_LOAD_DISTANCE_MARGIN;

    let mut to_retire: Vec<(Entity, LoadMmbRequest)> = Vec::new();
    let mut live_chunks: std::collections::HashSet<(u32, usize)> = Default::default();
    for (e, t, placement, slot) in placements_q.iter() {
        if slot.0 == crate::dat_mzb::ZONE_SLOT_MAIN
            && zone_placement_should_retire(center, t.translation(), load_radius)
        {
            to_retire.push((e, placement.0));
            continue;
        }
        live_chunks.insert((placement.0.file_id, placement.0.chunk_idx));
    }
    if to_retire.is_empty() {
        return;
    }

    for (e, req) in &to_retire {
        queue.pending.push_back(*req);
        if let Ok(mut ec) = commands.get_entity(*e) {
            ec.try_despawn();
        }
    }

    // A chunk is only dropped once every placement of it has retired; files are
    // further kept while any other chunk of theirs still streams or resides.
    let mut live_files: std::collections::HashSet<u32> = Default::default();
    for (file_id, _) in &live_chunks {
        live_files.insert(*file_id);
    }
    for (_, req) in &to_retire {
        if live_chunks.contains(&(req.file_id, req.chunk_idx)) {
            continue;
        }
        parse_cache.drop_chunk(req.file_id, req.chunk_idx);
        handle_cache.drop_chunk(req.file_id, req.chunk_idx);
        in_flight.tasks.remove(&(req.file_id, req.chunk_idx));
        if !live_files.contains(&req.file_id) {
            tex_pools.drop_file(req.file_id);
            drop_mmb_file_textures(req.file_id);
        }
    }
}

/// Choose an MMB submesh's render mode, per XIM (`research/xim` ·
/// `ZoneMeshSection.kt`). The model name (header bytes 16..32, our
/// Decode one file's texture set into pooled GPU images, shared by every chunk
/// parse of that DAT (the whole-file `Img` section is shared per file). The
/// first consumer of a file pays the decode; later ones hit the existing pool.
fn build_texture_pool(
    images: &mut Assets<Image>,
    by_file: &mut std::collections::HashMap<
        u32,
        (
            std::collections::HashMap<String, Handle<Image>>,
            Option<Handle<Image>>,
        ),
    >,
    file_id: u32,
    textures: &[NamedTexture],
    quality: TextureQuality,
) {
    by_file.entry(file_id).or_insert_with(|| {
        let mut by_name: std::collections::HashMap<String, Handle<Image>> =
            std::collections::HashMap::with_capacity(textures.len());
        let mut first: Option<Handle<Image>> = None;
        let mut tex_bytes = 0usize;
        let mut tex_px = 0usize;
        for (i, nt) in textures.iter().enumerate() {
            let img = decoded_texture_to_image(&nt.texture, quality);
            tex_bytes += img.data.as_ref().map(|d| d.len()).unwrap_or(0);
            tex_px += (nt.texture.width as usize) * (nt.texture.height as usize);
            let handle = images.add(img);
            if std::env::var("FFXI_DIAG_STREAM").is_ok() && i.is_multiple_of(5) {
                info!(
                    "DIAG mmb tex [{i}/{}] {}x{} rss_mb {}",
                    textures.len(),
                    nt.texture.width,
                    nt.texture.height,
                    diag_rss_mb()
                );
            }
            if first.is_none() {
                first = Some(handle.clone());
            }
            if !nt.name.is_empty() {
                by_name.insert(nt.name.clone(), handle);
            }
        }
        if std::env::var("FFXI_DIAG_STREAM").is_ok() {
            info!(
                "DIAG mmb texpool: fid {} n {} px {} bytes MB {:.1} rss_mb {}",
                file_id,
                textures.len(),
                tex_px,
                tex_bytes as f64 / 1_048_576.0,
                diag_rss_mb()
            );
        }
        (by_name, first)
    });
}

/// XIM's `discardThreshold` of 0.375; the `0x8000` flag bit marks translucency
/// (water/glass), rendered as `AlphaMode::Blend` — the zone shader emits real
/// alpha for these (see `flags.y` in `zone_ffxi.wgsl`). Everything else is
/// opaque. The render mode is NEVER derived from texture alpha content — doing
/// so punches holes in ordinary opaque ground/wall textures that carry
/// incidental transparency.
pub(crate) fn submesh_alpha_mode(
    zone_mesh_name: &str,
    blending: u16,
    has_texture: bool,
) -> (AlphaMode, f32) {
    if !has_texture {
        (AlphaMode::Opaque, 0.0)
    } else if zone_mesh_name.starts_with('_') {
        (AlphaMode::Mask(0.375), 0.375)
    } else if (blending & 0x8000) != 0 {
        (AlphaMode::Blend, 0.0)
    } else {
        (AlphaMode::Opaque, 0.0)
    }
}

fn push_system_msg(toasts: &mut MessageWriter<crate::snapshot::ToastEvent>, text: String) {
    toasts.write(crate::snapshot::ToastEvent::debug(text));
}

/// Re-sample the already-pooled MMB textures when the Texture Filtering setting
/// changes. Only the sampler's anisotropy varies live; the bilinear+mip data is
/// baked at load, so we patch the sampler in place rather than rebuild images.
/// The applied-value guard skips the GPU re-upload when an unrelated graphics
/// setting triggered the change.
pub fn apply_texture_filtering_system(
    settings: Res<GraphicsSettings>,
    pools: Res<MmbTexPools>,
    mut images: ResMut<Assets<Image>>,
    mut applied: ResMut<AppliedTextureFiltering>,
) {
    let aniso = settings.texture_filtering.anisotropy();
    if applied.anisotropy == Some(aniso) {
        return;
    }
    let mut patch = |handle: &Handle<Image>| {
        if let Some(mut img) = images.get_mut(handle) {
            img.sampler = bevy::image::ImageSampler::Descriptor(
                crate::zone_texture::sampler_descriptor(aniso),
            );
        }
    };
    for (by_name, first) in pools.by_file.values() {
        for handle in by_name.values() {
            patch(handle);
        }
        if let Some(handle) = first {
            patch(handle);
        }
    }
    applied.anisotropy = Some(aniso);
}

// ---------------------------------------------------------------------------
// Whole-zone textured bake (xi-model-viewer parity)
//
// A real in-game session (an `IsSelf` entity exists) gets its zone as ONE
// ordered static bake instead of per-chunk streaming geometry: every
// placement's MMB submeshes merge into a single world-space mesh per
// (chunk, submesh, mirror, sub-area) key, drawn with the author's texture and
// render state — the same order/state model as xi-model-viewer's zoneModel.js.
// The launcher backdrop flight (no `IsSelf`) keeps the budget-paced streaming,
// because a whole-zone bake under its panning camera stalls every frame with
// the zone build. Generator water sheets and `_`/`@` door leaves keep streaming
// even in bake mode: water wants its own animated material per sheet, and a
// door leaf's pose is animated per placement.
// ---------------------------------------------------------------------------

/// Baked-zone meshes are never distance-culled (the bake IS the whole zone, so
/// there is nothing cheaper to switch to); Bevy frustum-culls them instead.
#[derive(Component)]
pub struct BakedZoneMesh;

/// Whether the main-block bake for a zone file is still being assembled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BakeStatus {
    InProgress,
    Done,
}

/// Tracked main-block bake state, read by the floor/loading gate so the player
/// never spawns on a zone whose textured bake is still building.
#[derive(Default, Resource)]
pub struct ZoneBakeState {
    /// `(file_id, slot, done)`. `None` means the current zone never asked for
    /// a bake (backdrop flight or a non-baked load), and the gate is open.
    pub status: Option<(u32, u8, BakeStatus)>,
    pub active: Option<ActiveBake>,
}

impl ZoneBakeState {
    /// Loading/floor gate: ready unless THIS zone file has a bake still being
    /// assembled. No tracked bake (backdrop flight / non-baked load) and a
    /// finished bake are both ready; a bake for a different file is a
    /// zone-change race that only the reset path clears.
    pub fn main_bake_ready(&self, file_id: u32) -> bool {
        match self.status {
            Some((f, _, BakeStatus::InProgress)) => f != file_id,
            _ => true,
        }
    }

    /// Drop in-flight bake work when the zone changes (old tasks are cancelled
    /// by dropping them, old status cleared).
    pub fn clear(&mut self) {
        self.status = None;
        self.active = None;
    }
}

#[derive(bevy::ecs::system::SystemParam)]
pub struct ZoneBakeCtx<'w, 's> {
    pub real: Query<'w, 's, &'static GlobalTransform, With<crate::components::IsSelf>>,
    pub bake: ResMut<'w, ZoneBakeState>,
    pub parse: ResMut<'w, MmbParseCache>,
}

pub struct ActiveBake {
    pub file_id: u32,
    pub slot: u8,
    pub world_pos: Vec3,
    /// The filtered spawn list this bake covers, in placement order.
    pub spawns: Vec<crate::dat_mzb::ZoneMmbSpawn>,
    /// Distinct chunk indices in first-use order.
    pub chunks: Vec<usize>,
    pub done: std::collections::HashSet<usize>,
    pub tasks: Vec<(usize, Task<Option<LoadedMmb>>)>,
    /// How many `spawns` have been merged into `buckets` so far.
    pub merged: usize,
    /// Per-(chunk, submesh, mirror, sub-area) accumulated geometry.
    pub buckets: std::collections::HashMap<BakeBucketKey, BakeBucket>,
    /// Buckets awaiting entity spawn, in key-insertion order.
    pub pending: std::collections::VecDeque<BakeBucketKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BakeBucketKey {
    pub chunk_idx: usize,
    pub sub_index: usize,
    pub mirrored: bool,
    pub sub_area_link: u32,
}

pub struct BakeBucket {
    pub zone_mesh_name: String,
    pub variant_name: String,
    pub blending: u16,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
}

/// Per-frame cost caps. The bake runs under the floor gate, but the caps still
/// hold on every main-thread frame: merging a placement is a few hundred vertex
/// transforms; spawning a bucket is a Bevy mesh/material registration.
const BAKE_MERGE_PLACEMENTS_PER_FRAME: usize = 4096;
const BAKE_SPAWN_BUCKETS_PER_FRAME: usize = 24;

/// Start assembling the main-block bake for a zone that just loaded in a real
/// session. `spawns` has already been filtered: generator water and door-leaf
/// placements stay on the streaming path, and non-High LOD variants are dropped
/// (the bake holds the authored high-detail mesh at every distance).
pub fn kick_zone_bake(
    bake: &mut ZoneBakeState,
    parse: &mut MmbParseCache,
    file_id: u32,
    slot: u8,
    world_pos: Vec3,
    spawns: Vec<crate::dat_mzb::ZoneMmbSpawn>,
) {
    let mut chunks: Vec<usize> = Vec::new();
    for s in &spawns {
        if !chunks.contains(&s.chunk_idx) {
            chunks.push(s.chunk_idx);
        }
    }
    let pool = AsyncComputeTaskPool::get();
    let mut done: std::collections::HashSet<usize> = Default::default();
    let mut tasks: Vec<(usize, Task<Option<LoadedMmb>>)> = Vec::new();
    for &chunk_idx in &chunks {
        if parse.by_asset.contains_key(&(file_id, chunk_idx)) {
            done.insert(chunk_idx);
        } else {
            let key = (file_id, chunk_idx);
            tasks.push((
                chunk_idx,
                pool.spawn(async move { load_mmb(key.0, key.1).ok() }),
            ));
        }
    }
    if std::env::var("FFXI_DIAG_STREAM").is_ok() {
        info!(
            "DIAG zone bake: kick fid {} slot {} spawns {} distinct chunks {} tasks {} cached {} rss_mb {}",
            file_id,
            slot,
            spawns.len(),
            chunks.len(),
            tasks.len(),
            done.len(),
            diag_rss_mb()
        );
    }
    bake.status = Some((file_id, slot, BakeStatus::InProgress));
    bake.active = Some(ActiveBake {
        file_id,
        slot,
        world_pos,
        spawns,
        chunks,
        done,
        tasks,
        merged: 0,
        buckets: Default::default(),
        pending: Default::default(),
    });
}

fn merge_bake_placements(active: &mut ActiveBake, parse: &MmbParseCache, upto: usize) {
    while active.merged < upto && active.merged < active.spawns.len() {
        let s = active.spawns[active.merged];
        let Some(Some(loaded)) = parse.by_asset.get(&(active.file_id, s.chunk_idx)) else {
            active.merged += 1;
            continue;
        };
        let m = s.bevy_transform;
        let mirrored = m.determinant() < 0.0;
        for (sub_index, sub) in loaded.submeshes.iter().enumerate() {
            if sub.positions.is_empty() || sub.indices.is_empty() {
                continue;
            }
            let key = BakeBucketKey {
                chunk_idx: s.chunk_idx,
                sub_index,
                mirrored,
                sub_area_link: s.sub_area_link,
            };
            let bucket = active.buckets.entry(key).or_insert_with(|| BakeBucket {
                zone_mesh_name: loaded.zone_mesh_name.clone(),
                variant_name: sub.variant_name.trim().to_string(),
                blending: sub.blending,
                positions: Vec::new(),
                normals: Vec::new(),
                uvs: Vec::new(),
                colors: Vec::new(),
                indices: Vec::new(),
            });
            let base = bucket.positions.len() as u32;
            for p in &sub.positions {
                let wp = m.transform_point3(Vec3::from_array(*p));
                bucket.positions.push(wp.to_array());
            }
            if sub.normals.len() == sub.positions.len() {
                for n in &sub.normals {
                    let wn = m.transform_vector3(Vec3::from_array(*n)).normalize();
                    bucket.normals.push(wn.to_array());
                }
            }
            bucket.uvs.extend_from_slice(&sub.uvs);
            bucket.colors.extend_from_slice(&sub.colors);
            for &idx in &sub.indices {
                bucket.indices.push(base + idx);
            }
        }
        active.merged += 1;
    }
}

/// FfxiZoneMaterial for one merged bucket: the author's texture, vertex-mapped
/// palette, blending state and mirror pipeline, matching what the per-placement
/// streaming path builds for the same submesh (dat_mmb.rs `process_load_mmb_requests`).
fn spawn_baked_bucket(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<FfxiZoneMaterial>,
    images: &mut Assets<Image>,
    tex_pools: &mut MmbTexPools,
    parse: &MmbParseCache,
    file_id: u32,
    slot: u8,
    world_pos: Vec3,
    key: BakeBucketKey,
    bucket: BakeBucket,
    settings: &GraphicsSettings,
) {
    let Some(loaded) = parse
        .by_asset
        .get(&(file_id, key.chunk_idx))
        .and_then(|o| o.as_ref())
    else {
        return;
    };
    let quality = TextureQuality {
        mipmaps: settings.texture_filtering.mipmaps(),
        anisotropy: settings.texture_filtering.anisotropy(),
    };
    build_texture_pool(
        images,
        &mut tex_pools.by_file,
        file_id,
        &loaded.textures,
        quality,
    );
    let Some((by_name, first)) = tex_pools.by_file.get(&file_id) else {
        return;
    };
    let sub_texture = by_name
        .get(&bucket.variant_name)
        .cloned()
        .or_else(|| first.clone());

    let (alpha_mode, discard_threshold) = submesh_alpha_mode(
        &bucket.zone_mesh_name,
        bucket.blending,
        sub_texture.is_some(),
    );
    let has_texture = if sub_texture.is_some() { 1.0 } else { 0.0 };
    let blend_flag = if matches!(alpha_mode, AlphaMode::Blend) {
        1.0
    } else {
        0.0
    };
    let rs = ffxi_dat::mmb::MmbRenderState::from_blending(bucket.blending);
    let render_key = crate::ffxi_zone_material::FfxiZoneMaterialKey {
        back_face_culling: rs.back_face_culling,
        mirrored: key.mirrored,
        z_bias_level: rs.z_bias_level(),
        depth_write: rs.depth_write(),
        generator_stage_chain: false,
    };
    let material = materials.add(crate::ffxi_zone_material::FfxiZoneMaterial::new(
        sub_texture,
        crate::skinned_ffxi_material::FfxiMaterialFlags {
            flags: Vec4::new(
                has_texture,
                blend_flag,
                crate::ffxi_zone_material::ZONE_FLAG_FOGGED,
                discard_threshold,
            ),
        },
        Vec4::ONE,
        Vec4::ZERO,
        alpha_mode,
        render_key,
    ));

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, bucket.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, bucket.normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, bucket.uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, bucket.colors);
    mesh.insert_indices(Indices::U32(bucket.indices));
    let mesh_handle = meshes.add(mesh);

    let is_blend = matches!(alpha_mode, AlphaMode::Blend);
    let mut e = commands.spawn((
        MmbOverlay,
        BakedZoneMesh,
        crate::components::InGameEntity,
        crate::dat_mzb::AutoMzbOverlay,
        crate::dat_mzb::ZoneBlockSlot(slot),
        crate::dat_mzb::ZoneSubAreaLink(key.sub_area_link),
        Mesh3d(mesh_handle),
        MeshMaterial3d(material),
        Transform::from_translation(world_pos),
        Visibility::Inherited,
        crate::components::CameraOccluder,
        bevy::camera::visibility::RenderLayers::default()
            .with(crate::minimap::topdown::MINIMAP_BAKE_LAYER),
    ));
    if is_blend {
        e.insert((bevy::light::NotShadowCaster, bevy::light::NotShadowReceiver));
    }
}

/// One phase-step of the whole-zone bake: poll parses, merge placement
/// triangles into per-bucket world-space meshes, then spawn each bucket as a
/// single textured entity (budget-bounded so no main-thread frame stalls).
pub fn poll_zone_bake(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FfxiZoneMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut toasts: MessageWriter<crate::snapshot::ToastEvent>,
    mut bake: ResMut<ZoneBakeState>,
    mut parse: ResMut<MmbParseCache>,
    mut tex_pools: ResMut<MmbTexPools>,
    settings: Res<GraphicsSettings>,
) {
    let Some(active) = bake.active.take() else {
        return;
    };
    let mut active = active;

    let mut newly: Vec<(usize, Option<LoadedMmb>)> = Vec::new();
    let mut i = 0usize;
    while i < active.tasks.len() {
        let result = future::block_on(future::poll_once(&mut active.tasks[i].1));
        if let Some(loaded) = result {
            let (ci, _task) = active.tasks.swap_remove(i);
            newly.push((ci, loaded));
        } else {
            i += 1;
        }
    }
    for (chunk_idx, result) in newly {
        parse.by_asset.insert((active.file_id, chunk_idx), result);
        active.done.insert(chunk_idx);
    }

    if active.tasks.is_empty()
        && active.merged < active.spawns.len()
        && active.chunks.iter().all(|c| active.done.contains(c))
    {
        let upto = (active.merged + BAKE_MERGE_PLACEMENTS_PER_FRAME).min(active.spawns.len());
        merge_bake_placements(&mut active, &parse, upto);
    }

    if active.merged >= active.spawns.len() && active.pending.is_empty() {
        active.pending.extend(active.buckets.keys().copied());
    }

    let mut spawned_this_frame = 0usize;
    while spawned_this_frame < BAKE_SPAWN_BUCKETS_PER_FRAME {
        let Some(key) = active.pending.pop_front() else {
            break;
        };
        let Some(bucket) = active.buckets.remove(&key) else {
            continue;
        };
        spawn_baked_bucket(
            &mut commands,
            &mut meshes,
            &mut materials,
            &mut images,
            &mut tex_pools,
            &parse,
            active.file_id,
            active.slot,
            active.world_pos,
            key,
            bucket,
            &settings,
        );
        spawned_this_frame += 1;
    }

    if active.merged >= active.spawns.len()
        && active.pending.is_empty()
        && active.buckets.is_empty()
    {
        if std::env::var("FFXI_DIAG_STREAM").is_ok() {
            info!(
                "DIAG zone bake: done fid {} spawns {} rss_mb {}",
                active.file_id,
                active.spawns.len(),
                diag_rss_mb()
            );
        }
        push_system_msg(
            &mut toasts,
            format!(
                "/load_mzb {}: baked textured zone ({} placements)",
                active.file_id,
                active.spawns.len()
            ),
        );
        bake.status = Some((active.file_id, active.slot, BakeStatus::Done));
    } else {
        bake.active = Some(active);
    }
}

fn mesh_debug_bundle(
    info: crate::components::MmbDebugInfo,
) -> (bevy::picking::Pickable, crate::components::MmbDebugInfo) {
    // IGNORE, not default(): this bundle is attached to every MMB submesh
    // unconditionally, and a blocking Pickable::default() here made MMB props
    // swallow the world-picking ray so entity hitboxes behind them were never
    // hit (kuluu-k929). The off-by-default mesh-debug hover loses MMB hover as a
    // result; it would need its own MeshRayCast to survive opt-in picking.
    (bevy::picking::Pickable::IGNORE, info)
}

#[cfg(test)]
mod tests {
    use super::{
        mmb_dist_sq_xz, mmb_load_order_key, mmb_repass_needed, submesh_alpha_mode,
        zone_placement_should_retire, LoadMmbRequest, MMB_REEVAL_MOVE_YALMS,
    };
    use crate::zone_texture::ffxi_alpha_remap;
    use bevy::prelude::{AlphaMode, Mat4, Vec3};
    use ffxi_dat::mzb::NO_SUB_AREA_LINK;

    #[test]
    fn far_zone_placements_retire_beyond_the_hysteresis_margin() {
        let center = Vec3::new(100.0, 0.0, 100.0);
        let load_radius = 100.0;
        // Inside the spawn radius — never a retirement candidate.
        assert!(!zone_placement_should_retire(
            center,
            center + Vec3::new(80.0, 0.0, 0.0),
            load_radius
        ));
        // At 1.3x the spawn radius a placement stays resident: it spawned this
        // side of the radius and the 1.5x margin keeps the ring inside it from
        // being churned by boundary jitter (the hysteresis gap).
        assert!(!zone_placement_should_retire(
            center,
            center + Vec3::new(130.0, 0.0, 0.0),
            load_radius
        ));
        // Beyond the 1.5x margin it is retired.
        assert!(zone_placement_should_retire(
            center,
            center + Vec3::new(200.0, 0.0, 0.0),
            load_radius
        ));
        // Owner-side teleport: the placement that spawned within the radius but
        // is now far side of the margin retires.
        assert!(zone_placement_should_retire(
            center,
            center + Vec3::new(-200.0, 999.0, 0.0),
            load_radius
        ));
        // Y is ignored: a high-but-close structure over the center stays.
        assert!(!zone_placement_should_retire(
            center,
            center + Vec3::new(0.0, 5000.0, 0.0),
            load_radius
        ));
    }

    #[test]
    fn repass_triggers_on_events_parses_budget_or_settings() {
        assert!(mmb_repass_needed(true, false, false, false, None, None));
        assert!(mmb_repass_needed(false, true, false, false, None, None));
        assert!(mmb_repass_needed(false, false, true, false, None, None));
        assert!(mmb_repass_needed(false, false, false, true, None, None));
        assert!(!mmb_repass_needed(false, false, false, false, None, None));
    }

    #[test]
    fn repass_gates_on_reference_position_movement() {
        let prev = Some(Vec3::new(10.0, 0.0, 10.0));
        let near = Some(Vec3::new(10.0 + MMB_REEVAL_MOVE_YALMS * 0.5, 0.0, 10.0));
        let far = Some(Vec3::new(10.0 + MMB_REEVAL_MOVE_YALMS * 2.0, 0.0, 10.0));
        assert!(!mmb_repass_needed(false, false, false, false, prev, prev));
        assert!(!mmb_repass_needed(false, false, false, false, prev, near));
        assert!(mmb_repass_needed(false, false, false, false, prev, far));

        assert!(
            mmb_repass_needed(false, false, false, false, None, prev),
            "self appearing must re-evaluate"
        );
        assert!(
            mmb_repass_needed(false, false, false, false, prev, None),
            "self disappearing must re-evaluate"
        );
    }

    fn zone_placement_at(pos: Vec3) -> LoadMmbRequest {
        LoadMmbRequest {
            file_id: 0,
            chunk_idx: 0,
            world_pos: Vec3::ZERO,
            entity_id: None,
            world_transform: Some(Mat4::from_translation(pos)),
            water: None,
            lod: None,
            door: None,
            slot: crate::dat_mzb::ZONE_SLOT_MAIN,
            sub_area_link: NO_SUB_AREA_LINK,
        }
    }

    fn entity_spawn_at(pos: Vec3) -> LoadMmbRequest {
        LoadMmbRequest {
            file_id: 0,
            chunk_idx: 0,
            world_pos: pos,
            entity_id: Some(7),
            world_transform: None,
            water: None,
            lod: None,
            door: None,
            slot: crate::dat_mzb::ZONE_SLOT_MAIN,
            sub_area_link: NO_SUB_AREA_LINK,
        }
    }

    #[test]
    fn dist_key_ignores_vertical_axis() {
        let self_pos = Vec3::new(10.0, 999.0, 20.0);
        let req = zone_placement_at(Vec3::new(13.0, -50.0, 24.0));
        assert_eq!(mmb_dist_sq_xz(&req, self_pos), 3.0 * 3.0 + 4.0 * 4.0);
    }

    #[test]
    fn entity_spawns_sort_ahead_of_any_zone_placement() {
        let self_pos = Vec3::ZERO;
        let entity = mmb_load_order_key(&entity_spawn_at(Vec3::new(500.0, 0.0, 500.0)), self_pos);
        let nearest_prop =
            mmb_load_order_key(&zone_placement_at(Vec3::new(0.1, 0.0, 0.0)), self_pos);
        assert!(entity < nearest_prop);
    }

    #[test]
    fn nearer_zone_placement_sorts_first() {
        let self_pos = Vec3::ZERO;
        let near = mmb_load_order_key(&zone_placement_at(Vec3::new(5.0, 0.0, 0.0)), self_pos);
        let far = mmb_load_order_key(&zone_placement_at(Vec3::new(50.0, 0.0, 0.0)), self_pos);
        assert!(near < far);
    }

    #[test]
    fn shared_file_texture_sets_are_deduplicated_and_dropped() {
        use super::{drop_mmb_file_textures, mmb_file_textures_bank, NamedTexture};
        use ffxi_dat::texture::DecodedTexture;
        use std::sync::Arc;

        let tex = |w, h| NamedTexture {
            name: "t".to_string(),
            texture: DecodedTexture {
                width: w,
                height: h,
                format_tag: ffxi_dat::texture::TexFormat::Argb32,
                rgba: vec![0u8; (w as usize) * (h as usize) * 4],
            },
        };

        let bank = mmb_file_textures_bank();
        let set = Arc::new(vec![tex(4, 4), tex(2, 2)]);
        bank.lock().unwrap().insert(202, set.clone());

        // A second parse of the same DAT must observe the same shared set; two
        // chunks must never each decode (and own) the file's Img section.
        {
            let mut b = bank.lock().unwrap();
            let first = b.get(&202).cloned().unwrap();
            let again = b.entry(202).or_insert_with(|| Arc::new(Vec::new())).clone();
            assert!(Arc::ptr_eq(&first, &again));
        }

        drop_mmb_file_textures(202);
        assert!(
            !bank.lock().unwrap().contains_key(&202),
            "the whole file's texture set must drop when its last chunk retires"
        );
    }

    #[test]
    fn underscore_model_is_cutout_at_xim_threshold() {
        // XIM: name.startsWith("_") -> discardThreshold 0.375f.
        let (mode, t) = submesh_alpha_mode("_yashi", 0x0000, true);
        assert_eq!(mode, AlphaMode::Mask(0.375));
        assert_eq!(t, 0.375);
    }

    #[test]
    fn blend_flag_is_translucent_non_underscore_model() {
        // XIM `ZoneMeshSection` 0x8000 -> real alpha blend (water/glass), not a
        // cutout. discard threshold is 0.0 so the shader's mask test never fires.
        let (mode, t) = submesh_alpha_mode("kabuse_m", 0x8000, true);
        assert_eq!(mode, AlphaMode::Blend);
        assert_eq!(t, 0.0);
    }

    #[test]
    fn plain_ground_model_stays_opaque() {
        // The regression: a non-underscore model with a non-blend flag (e.g.
        // back-face-cull-disable 0x2000) and incidental texture alpha must NOT
        // be alpha-tested, or the ground gets punched into holes.
        let (mode, t) = submesh_alpha_mode("ground01", 0x2000, true);
        assert_eq!(mode, AlphaMode::Opaque);
        assert_eq!(t, 0.0);
        let (mode, _) = submesh_alpha_mode("ground01", 0x0000, true);
        assert_eq!(mode, AlphaMode::Opaque);
    }

    #[test]
    fn textureless_submesh_is_opaque_even_if_underscore() {
        let (mode, _) = submesh_alpha_mode("_yashi", 0x8000, false);
        assert_eq!(mode, AlphaMode::Opaque);
    }

    #[test]
    fn ffxi_alpha_remap_obeys_lotus_spec() {
        assert_eq!(ffxi_alpha_remap(0), 0);
        assert_eq!(ffxi_alpha_remap(15), 0);

        assert_eq!(ffxi_alpha_remap(128), 255);
        assert_eq!(ffxi_alpha_remap(136), 255);
        assert_eq!(ffxi_alpha_remap(255), 255);

        let mut prev = 0u8;
        for raw in 0u16..=255 {
            let cur = ffxi_alpha_remap(raw as u8);
            assert!(
                cur >= prev,
                "remap not monotonic at raw={raw}: prev={prev}, cur={cur}"
            );
            prev = cur;
        }

        for raw in 128u16..=255 {
            assert_eq!(
                ffxi_alpha_remap(raw as u8),
                255,
                "raw {raw} should saturate to 255"
            );
        }
    }
}
