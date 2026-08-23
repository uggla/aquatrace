use rstar::{AABB, PointDistance, RTree, RTreeObject};

use crate::{
    gpx_parser::haversine_m,
    types::{OsmWaterPoint, RoutePoint, RouteSummary, WaterPointResult},
};

#[derive(Debug, Clone)]
struct RouteSegment {
    start: [f64; 2],
    end: [f64; 2],
    start_m: f64,
    length_m: f64,
}

impl RTreeObject for RouteSegment {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [
                self.start[0].min(self.end[0]),
                self.start[1].min(self.end[1]),
            ],
            [
                self.start[0].max(self.end[0]),
                self.start[1].max(self.end[1]),
            ],
        )
    }
}

impl PointDistance for RouteSegment {
    fn distance_2(&self, point: &[f64; 2]) -> f64 {
        project_on_segment(self, *point).distance_to_route_m.powi(2)
    }
}

#[derive(Debug)]
struct RouteIndex {
    origin_lat: f64,
    origin_lon: f64,
    meters_per_lon: f64,
    segments: RTree<RouteSegment>,
}

impl RouteIndex {
    fn new(route: &RouteSummary) -> Option<Self> {
        let origin = *route.points.first()?;
        let origin_lat = origin.lat;
        let origin_lon = origin.lon;
        let meters_per_lon = 111_320.0 * origin_lat.to_radians().cos().abs().max(0.01);
        let mut cumulative_m = 0.0;
        let mut segments = Vec::new();

        for pair in route.points.windows(2) {
            let length_m = haversine_m(pair[0], pair[1]);
            if length_m > 0.0 {
                segments.push(RouteSegment {
                    start: metric_point(pair[0], origin_lat, origin_lon, meters_per_lon),
                    end: metric_point(pair[1], origin_lat, origin_lon, meters_per_lon),
                    start_m: cumulative_m,
                    length_m,
                });
                cumulative_m += length_m;
            }
        }

        (!segments.is_empty()).then(|| Self {
            origin_lat,
            origin_lon,
            meters_per_lon,
            segments: RTree::bulk_load(segments),
        })
    }

    fn project_point(&self, lat: f64, lon: f64) -> Option<Projection> {
        let point = metric_lat_lon(
            lat,
            lon,
            self.origin_lat,
            self.origin_lon,
            self.meters_per_lon,
        );
        self.segments
            .nearest_neighbor(point)
            .map(|segment| project_on_segment(segment, point))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    pub distance_to_route_m: f64,
    pub position_m: f64,
}

pub fn project_water_points(
    route: &RouteSummary,
    water_points: &[OsmWaterPoint],
    max_distance_m: f64,
) -> Vec<WaterPointResult> {
    let Some(index) = RouteIndex::new(route) else {
        return Vec::new();
    };

    let mut projected: Vec<_> = water_points
        .iter()
        .filter_map(|point| {
            let projection = index.project_point(point.lat, point.lon)?;
            (projection.distance_to_route_m <= max_distance_m).then(|| WaterPointResult {
                osm_type: point.osm_type,
                osm_id: point.osm_id,
                name: point.name.clone(),
                lat: point.lat,
                lon: point.lon,
                km: round1(projection.position_m / 1000.0),
                distance_to_route_m: projection.distance_to_route_m.round(),
            })
        })
        .collect();

    projected.sort_by(|a, b| {
        a.km.partial_cmp(&b.km)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.distance_to_route_m
                    .partial_cmp(&b.distance_to_route_m)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    projected
}

pub fn project_point(route: &RouteSummary, lat: f64, lon: f64) -> Option<Projection> {
    RouteIndex::new(route)?.project_point(lat, lon)
}

fn project_on_segment(segment: &RouteSegment, point: [f64; 2]) -> Projection {
    let dx = segment.end[0] - segment.start[0];
    let dy = segment.end[1] - segment.start[1];
    let length_sq = dx * dx + dy * dy;
    let t = if length_sq == 0.0 {
        0.0
    } else {
        (((point[0] - segment.start[0]) * dx + (point[1] - segment.start[1]) * dy) / length_sq)
            .clamp(0.0, 1.0)
    };

    let closest_x = segment.start[0] + t * dx;
    let closest_y = segment.start[1] + t * dy;
    let distance_to_route_m =
        ((point[0] - closest_x).powi(2) + (point[1] - closest_y).powi(2)).sqrt();

    Projection {
        distance_to_route_m,
        position_m: segment.start_m + segment.length_m * t,
    }
}

fn metric_point(
    point: RoutePoint,
    origin_lat: f64,
    origin_lon: f64,
    meters_per_lon: f64,
) -> [f64; 2] {
    metric_lat_lon(point.lat, point.lon, origin_lat, origin_lon, meters_per_lon)
}

fn metric_lat_lon(
    lat: f64,
    lon: f64,
    origin_lat: f64,
    origin_lon: f64,
    meters_per_lon: f64,
) -> [f64; 2] {
    [
        (lon - origin_lon) * meters_per_lon,
        (lat - origin_lat) * 111_320.0,
    ]
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::gpx_parser::{parse_gpx_route, summarize_route};

    use super::*;

    fn sample_route() -> RouteSummary {
        summarize_route(vec![
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: None,
            },
            RoutePoint {
                lat: 45.01,
                lon: 5.0,
                ele: None,
            },
        ])
    }

    #[test]
    fn projects_point_onto_route() {
        let route = sample_route();
        let projection = project_point(&route, 45.005, 5.001).unwrap();

        assert!((projection.position_m - route.distance_m / 2.0).abs() < 5.0);
        assert!((projection.distance_to_route_m - 78.7).abs() < 2.0);
    }

    #[test]
    fn computes_kilometer_and_filters_by_distance() {
        let route = sample_route();
        let points = vec![
            OsmWaterPoint {
                osm_type: crate::types::OsmElementType::Node,
                osm_id: 1,
                lat: 45.005,
                lon: 5.001,
                name: Some("Near".to_owned()),
            },
            OsmWaterPoint {
                osm_type: crate::types::OsmElementType::Node,
                osm_id: 2,
                lat: 45.005,
                lon: 5.02,
                name: Some("Far".to_owned()),
            },
        ];

        let projected = project_water_points(&route, &points, 200.0);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].osm_id, 1);
        assert!(projected[0].km > 0.5);
        assert!(projected[0].distance_to_route_m < 100.0);
    }

    #[test]
    fn projects_nearby_point_outside_exact_segment_envelope() {
        let route = sample_route();
        let projection = project_point(&route, 45.005, 5.002).unwrap();

        assert!((projection.position_m - route.distance_m / 2.0).abs() < 5.0);
        assert!(projection.distance_to_route_m > 150.0);
        assert!(projection.distance_to_route_m < 170.0);
    }

    #[test]
    fn finds_drinking_water_node_and_toilet_way_on_regression_trace() {
        let route = parse_gpx_route(include_bytes!(
            "../tests/fixtures/trace_toilette_eau_wq5ru.gpx"
        ))
        .unwrap();
        let points = vec![
            OsmWaterPoint {
                osm_type: crate::types::OsmElementType::Node,
                osm_id: 5_848_039_023,
                lat: 46.1599161,
                lon: 4.6794721,
                name: None,
            },
            OsmWaterPoint {
                osm_type: crate::types::OsmElementType::Way,
                osm_id: 140_994_931,
                lat: 46.16054,
                lon: 4.680389,
                name: None,
            },
        ];

        let projected = project_water_points(&route, &points, 500.0);
        assert_eq!(projected.len(), 2);
        assert!(projected.iter().any(|point| {
            point.osm_type == crate::types::OsmElementType::Node && point.osm_id == 5_848_039_023
        }));
        assert!(projected.iter().any(|point| {
            point.osm_type == crate::types::OsmElementType::Way && point.osm_id == 140_994_931
        }));
    }

    #[ignore = "performance regression fixture; run with --ignored --nocapture"]
    #[test]
    fn very_long_trace_projection_perf_regression() {
        let bytes = include_bytes!("../tests/fixtures/very_long_trace.gpx");
        let route = parse_gpx_route(bytes).unwrap();
        assert!(route.points.len() > 30_000);

        let water_points: Vec<_> = route
            .points
            .iter()
            .step_by(250)
            .enumerate()
            .map(|(index, point)| OsmWaterPoint {
                osm_type: crate::types::OsmElementType::Node,
                osm_id: i64::try_from(index).unwrap(),
                lat: point.lat,
                lon: point.lon,
                name: None,
            })
            .collect();

        let start = Instant::now();
        let projected = project_water_points(&route, &water_points, 500.0);
        let elapsed = start.elapsed();

        eprintln!(
            "projected {} water points on {} route points in {:?}",
            water_points.len(),
            route.points.len(),
            elapsed
        );
        assert_eq!(projected.len(), water_points.len());
        assert!(
            elapsed.as_secs() < 2,
            "long trace projection took {:?}",
            elapsed
        );
    }
}
