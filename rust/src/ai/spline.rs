//! The AI road network: every lane of a track's `ai/fast_lane*.ai` files
//! (or a `fast_lane.aip` package of them), joined into one graph of points
//! with links forward, back, left, right and across junctions. The file
//! formats and the linking rules are AssettoServer's, so its traffic tracks
//! load unchanged.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use rstar::RTree;
use rstar::primitives::GeomWithData;
use serde::Deserialize;
use smart_default::SmartDefault;

use crate::wire::{Reader, Vec3};

/// Status flags a junction can switch on (docs/03 §3.3).
pub const INDICATE_LEFT: u32 = 0x800;
pub const INDICATE_RIGHT: u32 = 0x1000;

/// One sample of a lane.
#[derive(Debug, Clone, PartialEq)]
pub struct SplinePoint {
    pub position: Vec3,
    /// Corner radius, metres.
    pub radius: f32,
    /// Road camber, radians.
    pub camber: f32,
    /// Distance to the next point.
    pub length: f32,
    pub previous: Option<u32>,
    pub next: Option<u32>,
    /// The point beside this one in the lane to the left / right.
    pub left: Option<u32>,
    pub right: Option<u32>,
    /// A junction that may branch off here, and one that joins here.
    pub junction_start: Option<u32>,
    pub junction_end: Option<u32>,
    /// This point's group of side-by-side lanes (`AiSpline::lanes`).
    pub lanes: Option<u32>,
}

/// A branch from one lane to another.
#[derive(Debug, Clone, PartialEq)]
pub struct Junction {
    pub start: u32,
    pub end: u32,
    /// How likely a car takes it.
    pub probability: f32,
    pub indicate_when_taken: u32,
    pub indicate_when_not_taken: u32,
    /// Indicate this far before the junction...
    pub indicate_distance_pre: f32,
    /// ...and this far after it.
    pub indicate_distance_post: f32,
}

/// `config.yml` next to the splines (AssettoServer's TrafficConfiguration).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct TrafficConfiguration {
    track: Option<String>,
    author: Option<String>,
    version: Option<i64>,
    splines: Vec<SplineConfiguration>,
}

#[derive(Debug, SmartDefault, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct SplineConfiguration {
    name: String,
    /// "<file>@<point>" this lane's end runs into.
    connect_end: Option<String>,
    indicate_end: Indicator,
    #[default(150.0)]
    indicate_end_distance_pre: f32,
    #[default(10.0)]
    indicate_end_distance_post: f32,
    junctions: Vec<JunctionRecord>,
}

#[derive(Debug, SmartDefault, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct JunctionRecord {
    name: String,
    /// The point in this lane where it starts.
    start: usize,
    /// "<file>@<point>" where it ends.
    end: String,
    probability: f32,
    indicate_when_taken: Indicator,
    indicate_when_not_taken: Indicator,
    #[default(75.0)]
    indicate_distance_pre: f32,
    #[default(50.0)]
    indicate_distance_post: f32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
enum Indicator {
    #[default]
    None,
    Left,
    Right,
}

impl Indicator {
    fn flags(self) -> u32 {
        match self {
            Indicator::None => 0,
            Indicator::Left => INDICATE_LEFT,
            Indicator::Right => INDICATE_RIGHT,
        }
    }
}

/// A YAML value read as AssettoServer's YAML library reads it: a key that
/// appears twice in one mapping keeps its last value instead of failing.
/// Published traffic configs (SRP's) contain such duplicates.
struct LenientValue(serde_norway::Value);

impl<'de> Deserialize<'de> for LenientValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LenientVisitor)
    }
}

struct LenientVisitor;

impl<'de> serde::de::Visitor<'de> for LenientVisitor {
    type Value = LenientValue;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<LenientValue, E> {
        Ok(LenientValue(v.into()))
    }
    fn visit_i64<E>(self, v: i64) -> Result<LenientValue, E> {
        Ok(LenientValue(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<LenientValue, E> {
        Ok(LenientValue(v.into()))
    }
    fn visit_f64<E>(self, v: f64) -> Result<LenientValue, E> {
        Ok(LenientValue(v.into()))
    }
    fn visit_str<E>(self, v: &str) -> Result<LenientValue, E> {
        Ok(LenientValue(v.into()))
    }
    fn visit_string<E>(self, v: String) -> Result<LenientValue, E> {
        Ok(LenientValue(v.into()))
    }
    fn visit_unit<E>(self) -> Result<LenientValue, E> {
        Ok(LenientValue(serde_norway::Value::Null))
    }
    fn visit_none<E>(self) -> Result<LenientValue, E> {
        Ok(LenientValue(serde_norway::Value::Null))
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<LenientValue, D::Error> {
        LenientValue::deserialize(d)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<LenientValue, A::Error> {
        let mut items = Vec::new();
        while let Some(LenientValue(v)) = seq.next_element()? {
            items.push(v);
        }
        Ok(LenientValue(serde_norway::Value::Sequence(items)))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<LenientValue, A::Error> {
        let mut mapping = serde_norway::Mapping::new();
        while let Some((LenientValue(k), LenientValue(v))) = map.next_entry()? {
            mapping.insert(k, v); // a repeated key: the last value wins
        }
        Ok(LenientValue(serde_norway::Value::Mapping(mapping)))
    }
}

/// One `.ai` file's points within `AiSpline::points`.
#[derive(Debug, Clone)]
struct Lane {
    first: u32,
    count: u32,
}

/// Finds the point nearest a position.
#[derive(Debug, Default)]
struct PointIndex(RTree<GeomWithData<[f32; 3], u32>>);

impl PointIndex {
    fn build(points: &[SplinePoint]) -> PointIndex {
        let entries = points
            .iter()
            .enumerate()
            .map(|(i, p)| GeomWithData::new(p.position.to_array(), i as u32))
            .collect();
        PointIndex(RTree::bulk_load(entries))
    }

    /// The nearest point within `max_distance`, with its squared distance.
    fn nearest(&self, at: Vec3, max_distance: f32) -> Option<(u32, f32)> {
        let nearest = self.0.nearest_neighbor(at.to_array())?;
        let d2 = Vec3::from_array(*nearest.geom()).distance_squared(at);
        (d2 <= max_distance * max_distance).then_some((nearest.data, d2))
    }
}

/// The road network.
#[derive(Debug)]
pub struct AiSpline {
    pub points: Vec<SplinePoint>,
    pub junctions: Vec<Junction>,
    /// Groups of side-by-side lane points, left to right.
    lanes: Vec<Vec<u32>>,
    index: PointIndex,
}

/// How far `world_to_spline` searches.
const MAX_SEARCH_DISTANCE: f32 = 1000.0;

/// Lane detection accepts a neighbour this close to where it expects one.
const LANE_DETECTION_RADIUS: f32 = 2.0;

/// Options that shape the network as it is built.
#[derive(Debug, Clone, Copy)]
pub struct BuildOptions {
    pub lane_width: f32,
    pub two_way_traffic: bool,
    pub smooth_camber: bool,
}

impl AiSpline {
    /// Loads `<track dir>/ai`: `fast_lane.aip` if present, else every
    /// `fast_lane*.ai` in name order, each with an optional `config.yml`.
    pub fn load(ai_dir: &Path, track: &str, options: BuildOptions) -> anyhow::Result<AiSpline> {
        let aip = ai_dir.join("fast_lane.aip");
        let (files, config_text) = if aip.exists() {
            read_package(&aip)?
        } else {
            read_folder(ai_dir)?
        };
        if files.is_empty() {
            bail!(
                "no AI splines: put fast_lane.ai(p) into {}",
                ai_dir.display()
            );
        }
        let config: Option<TrafficConfiguration> = match config_text {
            Some(text) => {
                let lenient: LenientValue =
                    serde_norway::from_str(&text).context("ai/config.yml")?;
                Some(serde_norway::from_value(lenient.0).context("ai/config.yml")?)
            }
            None => None,
        };
        if let Some(config) = &config {
            if let Some(author) = &config.author {
                tracing::info!(author, version = ?config.version, "AI spline");
            }
            if let Some(for_track) = &config.track {
                let ours = Path::new(track)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !for_track.is_empty() && *for_track != ours {
                    bail!("ai/config.yml is for track {for_track}, not {ours}");
                }
            }
        }
        let mut lanes_by_name: HashMap<String, Lane> = HashMap::new();
        let mut points: Vec<SplinePoint> = Vec::new();
        for (name, bytes) in &files {
            let first = points.len() as u32;
            let lane_points =
                parse_ai_file(bytes, first, options.smooth_camber).with_context(|| name.clone())?;
            tracing::debug!(file = %name, first, count = lane_points.len(), "AI spline parsed");
            lanes_by_name.insert(
                name.clone(),
                Lane {
                    first,
                    count: lane_points.len() as u32,
                },
            );
            points.extend(lane_points);
        }
        AiSpline::build(points, &lanes_by_name, config.as_ref(), options)
    }

    /// Links lanes side by side, groups them and applies junctions.
    fn build(
        points: Vec<SplinePoint>,
        lanes_by_name: &HashMap<String, Lane>,
        config: Option<&TrafficConfiguration>,
        options: BuildOptions,
    ) -> anyhow::Result<AiSpline> {
        let index = PointIndex::build(&points);
        let mut spline = AiSpline {
            points,
            junctions: Vec::new(),
            lanes: Vec::new(),
            index,
        };
        spline.detect_adjacent_lanes(options.lane_width)?;
        spline.group_lanes(options.two_way_traffic);
        if let Some(config) = config {
            spline.apply_configuration(config, lanes_by_name)?;
        }
        Ok(spline)
    }

    /// The nearest point to `at` within 1 km, with its squared distance.
    pub fn world_to_spline(&self, at: Vec3) -> Option<(u32, f32)> {
        self.index.nearest(at, MAX_SEARCH_DISTANCE)
    }

    pub fn point(&self, id: u32) -> &SplinePoint {
        &self.points[id as usize]
    }

    /// The vector to the next point, zero at a lane's end.
    pub fn forward(&self, id: u32) -> Vec3 {
        let p = self.point(id);
        match p.next {
            Some(next) => self.point(next).position - p.position,
            None => Vec3::default(),
        }
    }

    /// Whether two points' lanes run the same way.
    pub fn same_direction(&self, a: u32, b: u32) -> bool {
        self.forward(a).dot(self.forward(b)) > 0.0
    }

    /// Camber at `id`, blended `t` of the way to the next point.
    pub fn camber(&self, id: u32, t: f32) -> f32 {
        let p = self.point(id);
        match p.next {
            Some(next) if t != 0.0 => p.camber + (self.point(next).camber - p.camber) * t,
            _ => p.camber,
        }
    }

    /// The side-by-side lane points at `id`, itself included.
    pub fn lanes_at(&self, id: u32) -> &[u32] {
        match self.point(id).lanes {
            Some(group) => &self.lanes[group as usize],
            None => &[],
        }
    }

    /// For every point without a right neighbour, looks one lane width to
    /// each side for a point of another lane and links the two.
    fn detect_adjacent_lanes(&mut self, lane_width: f32) -> anyhow::Result<()> {
        let min_width = LANE_DETECTION_RADIUS * 1.05;
        if lane_width < min_width {
            bail!("lane width cannot be below {min_width} m");
        }
        for i in 0..self.points.len() as u32 {
            let point = self.point(i).clone();
            let (None, Some(next)) = (point.right, point.next) else {
                continue;
            };
            let next_pos = self.point(next).position;
            // Heading in degrees, as AssettoServer measures it.
            let direction = -((point.position.z - next_pos.z) as f64)
                .atan2((next_pos.x - point.position.x) as f64)
                .to_degrees();
            let probe = |angle: f64| -> Vec3 {
                let rad = angle.to_radians();
                Vec3::new(
                    (f64::from(point.position.x) + rad.cos() * f64::from(lane_width)) as f32,
                    point.position.y,
                    (f64::from(point.position.z) - rad.sin() * f64::from(lane_width)) as f32,
                )
            };
            let near = |target: Vec3| {
                self.index
                    .nearest(target, LANE_DETECTION_RADIUS)
                    .filter(|&(_, d2)| d2 < LANE_DETECTION_RADIUS * LANE_DETECTION_RADIUS)
                    .map(|(id, _)| id)
            };
            let left = near(probe(-direction + 90.0));
            let right = near(probe(-direction - 90.0));
            if let Some(found) = left {
                self.points[i as usize].left = Some(found);
                if self.same_direction(i, found) {
                    self.points[found as usize].right = Some(i);
                } else {
                    self.points[found as usize].left = Some(i);
                }
            }
            if let Some(found) = right {
                self.points[i as usize].right = Some(found);
                if self.same_direction(i, found) {
                    self.points[found as usize].left = Some(i);
                } else {
                    self.points[found as usize].right = Some(i);
                }
            }
        }
        Ok(())
    }

    /// Walks left then right from `start` across lanes running its way (and
    /// the other way with two-way traffic), at most 10.
    fn side_by_side(&self, start: u32, two_way: bool) -> Vec<u32> {
        const MAX_LANES: usize = 10;
        let mut left_side = Vec::new();
        // An opposite lane's own left and right are mirrored, so from one the
        // walk steps the other way.
        let walk = |first: Option<u32>, out: &mut Vec<u32>, toward_left: bool| {
            let mut at = first;
            while let Some(id) = at {
                if out.len() >= MAX_LANES || out.contains(&id) || id == start {
                    break;
                }
                let same = self.same_direction(start, id);
                if !same && !two_way {
                    break;
                }
                out.push(id);
                let p = self.point(id);
                at = if toward_left == same { p.left } else { p.right };
            }
        };
        walk(self.point(start).left, &mut left_side, true);
        left_side.reverse();
        let mut lanes = left_side;
        lanes.push(start);
        let mut right_side = Vec::new();
        walk(self.point(start).right, &mut right_side, false);
        lanes.extend(right_side);
        lanes.truncate(MAX_LANES);
        lanes
    }

    fn group_lanes(&mut self, two_way: bool) {
        for i in 0..self.points.len() as u32 {
            if self.point(i).lanes.is_some() {
                continue;
            }
            let group = self.side_by_side(i, two_way);
            let index = self.lanes.len() as u32;
            for &id in &group {
                self.points[id as usize].lanes = Some(index);
            }
            self.lanes.push(group);
        }
    }

    /// "<file>@<point>" as a point id.
    fn resolve(&self, lanes: &HashMap<String, Lane>, identifier: &str) -> anyhow::Result<u32> {
        let (file, index) = identifier
            .split_once('@')
            .with_context(|| format!("bad spline point {identifier:?}, want <file>@<index>"))?;
        let lane = lanes
            .get(file)
            .with_context(|| format!("no spline named {file}"))?;
        let index: u32 = index
            .parse()
            .with_context(|| format!("bad point index in {identifier:?}"))?;
        if index >= lane.count {
            bail!("{identifier:?} is past the end of {file}");
        }
        Ok(lane.first + index)
    }

    fn apply_configuration(
        &mut self,
        config: &TrafficConfiguration,
        lanes: &HashMap<String, Lane>,
    ) -> anyhow::Result<()> {
        for spline in &config.splines {
            let lane = lanes
                .get(&spline.name)
                .with_context(|| format!("config.yml: no spline named {}", spline.name))?
                .clone();
            if let Some(connect) = &spline.connect_end {
                let end = self.resolve(lanes, connect)?;
                let start = lane.first + lane.count - 1;
                self.points[start as usize].next = Some(end);
                self.add_junction(Junction {
                    start,
                    end,
                    probability: 1.0,
                    indicate_when_taken: spline.indicate_end.flags(),
                    indicate_when_not_taken: 0,
                    indicate_distance_pre: spline.indicate_end_distance_pre,
                    indicate_distance_post: spline.indicate_end_distance_post,
                });
            }
            for record in &spline.junctions {
                if record.start as u32 >= lane.count {
                    bail!(
                        "config.yml: junction {} starts past the end of {}",
                        record.name,
                        spline.name
                    );
                }
                let end = self.resolve(lanes, &record.end)?;
                tracing::debug!(name = %record.name, from = %spline.name, start = record.start, end = %record.end, "AI junction");
                self.add_junction(Junction {
                    start: lane.first + record.start as u32,
                    end,
                    probability: record.probability,
                    indicate_when_taken: record.indicate_when_taken.flags(),
                    indicate_when_not_taken: record.indicate_when_not_taken.flags(),
                    indicate_distance_pre: record.indicate_distance_pre,
                    indicate_distance_post: record.indicate_distance_post,
                });
            }
        }
        Ok(())
    }

    fn add_junction(&mut self, junction: Junction) {
        let id = self.junctions.len() as u32;
        self.points[junction.start as usize].junction_start = Some(id);
        self.points[junction.end as usize].junction_end = Some(id);
        self.junctions.push(junction);
    }
}

/// The `.ai` files in a folder, by name, plus its `config.yml`.
type SplineFiles = (Vec<(String, Vec<u8>)>, Option<String>);

fn read_folder(dir: &Path) -> anyhow::Result<SplineFiles> {
    if !dir.is_dir() {
        bail!("no ai folder: put fast_lane.ai(p) into {}", dir.display());
    }
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.starts_with("fast_lane") && name.ends_with(".ai") {
            names.push(name);
        }
    }
    // Sorted, so point ids (and the junctions naming them) are stable.
    names.sort();
    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let bytes = std::fs::read(dir.join(&name)).with_context(|| name.clone())?;
        files.push((name, bytes));
    }
    let config = std::fs::read_to_string(dir.join("config.yml")).ok();
    Ok((files, config))
}

/// A `fast_lane.aip`: a zip of `.ai` files and an optional `config.yml`.
fn read_package(path: &Path) -> anyhow::Result<SplineFiles> {
    tracing::info!(path = %path.display(), "loading AI package");
    let file = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let mut zip = zip::ZipArchive::new(file).context("fast_lane.aip is not a zip")?;
    let mut files = Vec::new();
    let mut config = None;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw_name = entry.name()?.to_string();
        let name = Path::new(&raw_name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if name == "config.yml" {
            config = Some(String::from_utf8_lossy(&bytes).into_owned());
        } else if name.ends_with(".ai") {
            files.push((name, bytes));
        }
    }
    Ok((files, config))
}

/// The most points one `.ai` file may declare, which bounds the allocation
/// a corrupt count could ask for.
const MAX_FILE_POINTS: i32 = 10_000_000;

/// Parses one `.ai` file: version 7 (Kunos) or -1 (AssettoServer's compact
/// form). `first_id` is the id of its first point in the whole network.
pub fn parse_ai_file(
    bytes: &[u8],
    first_id: u32,
    smooth_camber: bool,
) -> anyhow::Result<Vec<SplinePoint>> {
    let mut r = Reader::new(bytes);
    let version = r.i32();
    let count = r.i32();
    if r.overflowed() || !(2..=MAX_FILE_POINTS).contains(&count) {
        bail!("bad spline header");
    }
    let count = count as usize;
    let blank = SplinePoint {
        position: Vec3::default(),
        radius: 0.0,
        camber: 0.0,
        length: 0.0,
        previous: None,
        next: None,
        left: None,
        right: None,
        junction_start: None,
        junction_end: None,
        lanes: None,
    };
    let mut points = vec![blank; count];
    match version {
        7 => {
            let _lap_time = r.i32();
            let _sample_count = r.i32();
            for p in points.iter_mut() {
                p.position = r.vec3();
                let _length = r.f32();
                let _id = r.i32();
            }
            let extra_count = r.i32();
            if extra_count as usize != count {
                bail!("spline point count {count} does not match extra count {extra_count}");
            }
            for p in points.iter_mut() {
                let _speed = r.f32();
                let _gas = r.f32();
                let _brake = r.f32();
                let _obsolete_lat_g = r.f32();
                p.radius = r.f32();
                let _side_left = r.f32();
                let _side_right = r.f32();
                let camber = r.f32();
                let direction = r.f32(); // 1 or -1
                p.camber = camber * direction;
                let _normal = r.vec3();
                let _length = r.f32();
                let _forward = r.vec3();
                let _tag = r.f32();
                let _grade = r.f32();
            }
        }
        -1 => {
            for p in points.iter_mut() {
                p.position = r.vec3();
                p.radius = r.f32();
                p.camber = r.f32();
            }
        }
        other => bail!("unknown spline version {other}"),
    }
    if r.overflowed() {
        bail!("spline file is truncated");
    }

    // A point-to-point spline's last radius can be junk.
    if points[count - 1].radius < 1.0 {
        points[count - 1].radius = 1000.0;
    }
    let mut window: Vec<f32> = Vec::with_capacity(5);
    for i in 0..count {
        let next = if i == count - 1 { 0 } else { i + 1 };
        let previous = if i == 0 { count - 1 } else { i - 1 };
        points[i].previous = Some(first_id + previous as u32);
        points[i].next = Some(first_id + next as u32);
        points[i].length = points[i].position.distance(points[next].position);
        if smooth_camber {
            // A moving average over the last five points.
            if window.len() == 5 {
                window.remove(0);
            }
            window.push(points[i].camber);
            points[i].camber = window.iter().sum::<f32>() / window.len() as f32;
        }
    }
    // A lane whose ends are over 50 m apart is not a loop.
    if points[0].position.distance(points[count - 1].position) >= 50.0 {
        points[0].previous = None;
        points[count - 1].next = None;
        points[count - 1].length = 1.0;
    }
    Ok(points)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A straight lane along +x at `z`, as a version -1 file.
    pub(crate) fn straight_lane(z: f32, points: usize, step: f32, reverse: bool) -> Vec<u8> {
        let mut w = crate::wire::Writer::new();
        w.i32(-1);
        w.i32(points as i32);
        for i in 0..points {
            let i = if reverse { points - 1 - i } else { i };
            w.vec3(Vec3::new(i as f32 * step, 0.0, z));
            w.f32(1000.0);
            w.f32(0.0);
        }
        w.into_bytes()
    }

    fn options() -> BuildOptions {
        BuildOptions {
            lane_width: 3.0,
            two_way_traffic: false,
            smooth_camber: false,
        }
    }

    pub(crate) fn two_lane_road() -> AiSpline {
        let a = parse_ai_file(&straight_lane(0.0, 200, 2.0, false), 0, false).expect("lane a");
        let b = parse_ai_file(&straight_lane(3.0, 200, 2.0, false), 200, false).expect("lane b");
        let mut lanes = HashMap::new();
        lanes.insert(
            "fast_lane.ai".to_string(),
            Lane {
                first: 0,
                count: 200,
            },
        );
        lanes.insert(
            "fast_lane2.ai".to_string(),
            Lane {
                first: 200,
                count: 200,
            },
        );
        let mut points = a;
        points.extend(b);
        AiSpline::build(points, &lanes, None, options()).expect("build")
    }

    /// Loads a real traffic package: AI_SPLINE_DIR=<folder with
    /// fast_lane.aip or fast_lane*.ai>, AI_SPLINE_TRACK=<track id>.
    #[test]
    fn real_spline_loads() {
        let Ok(dir) = std::env::var("AI_SPLINE_DIR") else {
            return;
        };
        let track = std::env::var("AI_SPLINE_TRACK").unwrap_or_default();
        let started = std::time::Instant::now();
        let spline = match AiSpline::load(Path::new(&dir), &track, options()) {
            Ok(s) => s,
            Err(e) => panic!("load: {e:#}"),
        };
        let linked = spline
            .points
            .iter()
            .filter(|p| p.left.is_some() || p.right.is_some())
            .count();
        let ends = spline.points.iter().filter(|p| p.next.is_none()).count();
        let max_lanes = spline.lanes.iter().map(Vec::len).max().unwrap_or(0);
        println!(
            "points {} junctions {} lane groups {} widest {} linked {} ({:.0}%) dead ends {} in {:?}",
            spline.points.len(),
            spline.junctions.len(),
            spline.lanes.len(),
            max_lanes,
            linked,
            100.0 * linked as f64 / spline.points.len() as f64,
            ends,
            started.elapsed()
        );
        assert!(!spline.points.is_empty());
    }

    #[test]
    fn version_minus_one_parses_and_links() {
        let points = parse_ai_file(&straight_lane(0.0, 40, 2.0, false), 5, false).expect("parse");
        assert_eq!(points.len(), 40);
        assert_eq!(points[0].previous, None, "ends 78 m apart: not a loop");
        assert_eq!(points[0].next, Some(6));
        assert_eq!(points[39].next, None);
        assert!((points[3].length - 2.0).abs() < 1e-6);
        let ring = parse_ai_file(&straight_lane(0.0, 10, 2.0, false), 0, false).expect("parse");
        assert_eq!(ring[0].previous, Some(9), "ends 18 m apart: a loop");
        assert_eq!(ring[9].next, Some(0));
    }

    #[test]
    fn neighbouring_lanes_link_and_group() {
        let spline = two_lane_road();
        // Lane b (z = 3) is to the right of lane a (z = 0) going +x: AC's
        // world has z growing to the driver's right when facing +x.
        let a = spline.point(50);
        assert!(a.left.is_some() || a.right.is_some(), "lanes were linked");
        assert_eq!(spline.lanes_at(50).len(), 2);
        assert!(spline.lanes_at(50).contains(&250));
    }

    #[test]
    fn nearest_point_search() {
        let spline = two_lane_road();
        let (id, d2) = spline
            .world_to_spline(Vec3::new(100.4, 0.0, 0.2))
            .expect("found");
        assert_eq!(id, 50);
        assert!(d2 < 0.5);
        assert!(
            spline
                .world_to_spline(Vec3::new(5000.0, 0.0, 0.0))
                .is_none()
        );
    }

    #[test]
    fn duplicate_keys_keep_the_last() {
        let text = "Splines:\n  - Name: a.ai\n    Junctions:\n      - Name: j\n        Start: 1\n        End: b.ai@0\n        IndicateWhenNotTaken: Left\n        IndicateWhenNotTaken: Right\n";
        let lenient: LenientValue = serde_norway::from_str(text).expect("parses");
        let config: TrafficConfiguration = serde_norway::from_value(lenient.0).expect("typed");
        assert_eq!(
            config.splines[0].junctions[0]
                .indicate_when_not_taken
                .flags(),
            INDICATE_RIGHT
        );
    }

    #[test]
    fn junction_from_config() {
        let a = parse_ai_file(&straight_lane(0.0, 100, 2.0, false), 0, false).expect("a");
        let b = parse_ai_file(&straight_lane(40.0, 100, 2.0, false), 100, false).expect("b");
        let mut lanes = HashMap::new();
        lanes.insert(
            "fast_lane.ai".to_string(),
            Lane {
                first: 0,
                count: 100,
            },
        );
        lanes.insert(
            "fast_lane_exit.ai".to_string(),
            Lane {
                first: 100,
                count: 100,
            },
        );
        let config: TrafficConfiguration = serde_norway::from_str(
            "Splines:\n  - Name: fast_lane.ai\n    Junctions:\n      - Name: exit\n        Start: 20\n        End: fast_lane_exit.ai@5\n        Probability: 0.5\n        IndicateWhenTaken: Left\n",
        )
        .expect("yaml");
        let mut points = a;
        points.extend(b);
        let spline = AiSpline::build(points, &lanes, Some(&config), options()).expect("build");
        assert_eq!(spline.junctions.len(), 1);
        assert_eq!(spline.point(20).junction_start, Some(0));
        assert_eq!(spline.point(105).junction_end, Some(0));
        assert_eq!(spline.junctions[0].indicate_when_taken, INDICATE_LEFT);
        assert_eq!(spline.junctions[0].indicate_distance_pre, 75.0);
    }
}
