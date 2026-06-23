use std::io::Cursor;

use anyhow::{Context, Result, anyhow};

use crate::types::{RoutePoint, RouteSummary};

pub fn parse_gpx_route(bytes: &[u8]) -> Result<RouteSummary> {
    let gpx = gpx::read(Cursor::new(bytes)).context("invalid GPX document")?;
    let mut points = Vec::new();

    for track in gpx.tracks {
        for segment in track.segments {
            for point in segment.points {
                let geo_point = point.point();
                points.push(RoutePoint {
                    lat: geo_point.y(),
                    lon: geo_point.x(),
                    ele: point.elevation,
                });
            }
        }
    }

    if points.len() < 2 {
        return Err(anyhow!("GPX route must contain at least two track points"));
    }

    Ok(summarize_route(points))
}

pub fn summarize_route(points: Vec<RoutePoint>) -> RouteSummary {
    let mut distance_m = 0.0;
    let mut elevation_gain_m = 0.0;
    let mut elevation_loss_m = 0.0;
    let mut min_elevation_m: Option<f64> = None;
    let mut max_elevation_m: Option<f64> = None;

    for point in &points {
        if let Some(ele) = point.ele {
            min_elevation_m = Some(min_elevation_m.map_or(ele, |current| current.min(ele)));
            max_elevation_m = Some(max_elevation_m.map_or(ele, |current| current.max(ele)));
        }
    }

    for pair in points.windows(2) {
        distance_m += haversine_m(pair[0], pair[1]);

        if let (Some(previous), Some(next)) = (pair[0].ele, pair[1].ele) {
            let delta = next - previous;
            if delta > 0.0 {
                elevation_gain_m += delta;
            } else {
                elevation_loss_m += -delta;
            }
        }
    }

    RouteSummary {
        distance_m,
        elevation_gain_m,
        elevation_loss_m,
        min_elevation_m,
        max_elevation_m,
        points,
    }
}

pub fn haversine_m(a: RoutePoint, b: RoutePoint) -> f64 {
    let radius_m = 6_371_000.0;
    let lat1 = a.lat.to_radians();
    let lat2 = b.lat.to_radians();
    let dlat = (b.lat - a.lat).to_radians();
    let dlon = (b.lon - a.lon).to_radians();

    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);

    2.0 * radius_m * h.sqrt().atan2((1.0 - h).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_distance_and_elevation() {
        let summary = summarize_route(vec![
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: Some(100.0),
            },
            RoutePoint {
                lat: 45.001,
                lon: 5.0,
                ele: Some(130.0),
            },
            RoutePoint {
                lat: 45.002,
                lon: 5.0,
                ele: Some(90.0),
            },
        ]);

        assert!((summary.distance_m - 222.4).abs() < 1.0);
        assert_eq!(summary.elevation_gain_m, 30.0);
        assert_eq!(summary.elevation_loss_m, 40.0);
        assert_eq!(summary.min_elevation_m, Some(90.0));
        assert_eq!(summary.max_elevation_m, Some(130.0));
    }

    #[test]
    fn rejects_empty_tracks() {
        let gpx = br#"<?xml version="1.0"?><gpx version="1.1" creator="test"></gpx>"#;
        assert!(parse_gpx_route(gpx).is_err());
    }

    #[test]
    fn parses_track_points() {
        let gpx = br#"<?xml version="1.0"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="45.0" lon="5.0"><ele>100</ele></trkpt>
    <trkpt lat="45.001" lon="5.0"><ele>110</ele></trkpt>
  </trkseg></trk>
</gpx>"#;

        let summary = parse_gpx_route(gpx).unwrap();
        assert_eq!(summary.points.len(), 2);
        assert_eq!(summary.elevation_gain_m, 10.0);
    }
}
