use rstar::{AABB, RTree, RTreeObject};

use crate::{
    gpx_parser::haversine_m,
    types::{OsmWaterPoint, RoutePoint, RouteSummary, WaterPointResult},
};

#[derive(Debug, Clone)]
struct RouteSegment {
    start: RoutePoint,
    end: RoutePoint,
    start_m: f64,
    length_m: f64,
}

impl RTreeObject for RouteSegment {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [
                self.start.lon.min(self.end.lon),
                self.start.lat.min(self.end.lat),
            ],
            [
                self.start.lon.max(self.end.lon),
                self.start.lat.max(self.end.lat),
            ],
        )
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
    let mut projected: Vec<_> = water_points
        .iter()
        .filter_map(|point| {
            let projection = project_point(route, point.lat, point.lon)?;
            (projection.distance_to_route_m <= max_distance_m).then(|| WaterPointResult {
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
    let segments = build_segments(route);
    if segments.is_empty() {
        return None;
    }

    let tree = RTree::bulk_load(segments.clone());
    let point_bbox = AABB::from_point([lon, lat]);
    let mut candidates: Vec<&RouteSegment> =
        tree.locate_in_envelope_intersecting(point_bbox).collect();

    if candidates.is_empty() {
        candidates = segments.iter().collect();
    }

    candidates
        .into_iter()
        .map(|segment| project_on_segment(segment, lat, lon))
        .min_by(|a, b| {
            a.distance_to_route_m
                .partial_cmp(&b.distance_to_route_m)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn build_segments(route: &RouteSummary) -> Vec<RouteSegment> {
    let mut segments = Vec::new();
    let mut cumulative_m = 0.0;

    for pair in route.points.windows(2) {
        let length_m = haversine_m(pair[0], pair[1]);
        if length_m > 0.0 {
            segments.push(RouteSegment {
                start: pair[0],
                end: pair[1],
                start_m: cumulative_m,
                length_m,
            });
            cumulative_m += length_m;
        }
    }

    segments
}

fn project_on_segment(segment: &RouteSegment, lat: f64, lon: f64) -> Projection {
    let origin_lat = segment.start.lat;
    let meters_per_lon = 111_320.0 * origin_lat.to_radians().cos().abs().max(0.01);
    let meters_per_lat = 111_320.0;

    let ax = 0.0;
    let ay = 0.0;
    let bx = (segment.end.lon - segment.start.lon) * meters_per_lon;
    let by = (segment.end.lat - segment.start.lat) * meters_per_lat;
    let px = (lon - segment.start.lon) * meters_per_lon;
    let py = (lat - segment.start.lat) * meters_per_lat;

    let dx = bx - ax;
    let dy = by - ay;
    let length_sq = dx * dx + dy * dy;
    let t = if length_sq == 0.0 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / length_sq).clamp(0.0, 1.0)
    };

    let closest_x = ax + t * dx;
    let closest_y = ay + t * dy;
    let distance_to_route_m = ((px - closest_x).powi(2) + (py - closest_y).powi(2)).sqrt();

    Projection {
        distance_to_route_m,
        position_m: segment.start_m + segment.length_m * t,
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use crate::gpx_parser::summarize_route;

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
                osm_id: 1,
                lat: 45.005,
                lon: 5.001,
                name: Some("Near".to_owned()),
            },
            OsmWaterPoint {
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
}
