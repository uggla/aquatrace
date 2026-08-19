use std::io::Cursor;

use anyhow::{Context, Result, anyhow};
use gpx::errors::GpxError;

use crate::types::{RoutePoint, RouteSummary};

const ELEVATION_DEADBAND_M: f64 = 10.0;

pub fn parse_gpx_route(bytes: &[u8]) -> Result<RouteSummary> {
    let gpx = read_gpx_tolerating_empty_metadata(bytes).context("invalid GPX document")?;
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

fn read_gpx_tolerating_empty_metadata(bytes: &[u8]) -> Result<gpx::Gpx, GpxError> {
    match gpx::read(Cursor::new(bytes)) {
        Err(GpxError::NoStringContent) => {
            let Some(normalized) = remove_empty_license_elements(bytes) else {
                return Err(GpxError::NoStringContent);
            };
            gpx::read(Cursor::new(normalized))
        }
        result => result,
    }
}

fn remove_empty_license_elements(bytes: &[u8]) -> Option<Vec<u8>> {
    const OPENING_TAG: &[u8] = b"<license";
    const CLOSING_TAG: &[u8] = b"</license>";

    let mut output = Vec::with_capacity(bytes.len());
    let mut copy_from = 0;
    let mut search_from = 0;
    let mut changed = false;

    while let Some(relative_start) = find_bytes(&bytes[search_from..], OPENING_TAG) {
        let start = search_from + relative_start;
        let boundary = bytes.get(start + OPENING_TAG.len()).copied();
        if !boundary.is_some_and(|byte| byte == b'>' || byte == b'/' || byte.is_ascii_whitespace())
        {
            search_from = start + OPENING_TAG.len();
            continue;
        }

        let Some(relative_tag_end) = bytes[start..].iter().position(|byte| *byte == b'>') else {
            break;
        };
        let tag_end = start + relative_tag_end;
        let opening_content = &bytes[start + 1..tag_end];
        let self_closing = opening_content
            .iter()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|byte| *byte == b'/');

        let remove_end = if self_closing {
            Some(tag_end + 1)
        } else {
            let content_start = tag_end + 1;
            find_bytes(&bytes[content_start..], CLOSING_TAG).and_then(|relative_close| {
                let close_start = content_start + relative_close;
                bytes[content_start..close_start]
                    .iter()
                    .all(|byte| byte.is_ascii_whitespace())
                    .then_some(close_start + CLOSING_TAG.len())
            })
        };

        if let Some(remove_end) = remove_end {
            output.extend_from_slice(&bytes[copy_from..start]);
            copy_from = remove_end;
            search_from = remove_end;
            changed = true;
        } else {
            search_from = tag_end + 1;
        }
    }

    if !changed {
        return None;
    }

    output.extend_from_slice(&bytes[copy_from..]);
    Some(output)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn summarize_route(points: Vec<RoutePoint>) -> RouteSummary {
    let mut distance_m = 0.0;
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
    }

    let (elevation_gain_m, elevation_loss_m) = elevation_gain_loss(&points);

    RouteSummary {
        distance_m,
        elevation_gain_m,
        elevation_loss_m,
        min_elevation_m,
        max_elevation_m,
        points,
    }
}

fn elevation_gain_loss(points: &[RoutePoint]) -> (f64, f64) {
    let mut elevations = points.iter().filter_map(|point| point.ele);
    let Some(first) = elevations.next() else {
        return (0.0, 0.0);
    };

    let mut gain = 0.0;
    let mut loss = 0.0;
    let mut anchor = first;
    let mut high = first;
    let mut low = first;
    let mut direction = 0_i8;

    for ele in elevations {
        if direction >= 0 {
            high = high.max(ele);
            if high - ele >= ELEVATION_DEADBAND_M {
                gain += high - anchor;
                anchor = high;
                low = ele;
                direction = -1;
            }
        }

        if direction <= 0 {
            low = low.min(ele);
            if ele - low >= ELEVATION_DEADBAND_M {
                loss += anchor - low;
                anchor = low;
                high = ele;
                direction = 1;
            }
        }
    }

    if direction >= 0 {
        gain += high - anchor;
    }
    if direction <= 0 {
        loss += anchor - low;
    }

    (gain, loss)
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
    fn ignores_small_elevation_noise() {
        let summary = summarize_route(vec![
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: Some(100.0),
            },
            RoutePoint {
                lat: 45.001,
                lon: 5.0,
                ele: Some(101.5),
            },
            RoutePoint {
                lat: 45.002,
                lon: 5.0,
                ele: Some(100.4),
            },
            RoutePoint {
                lat: 45.003,
                lon: 5.0,
                ele: Some(102.0),
            },
            RoutePoint {
                lat: 45.004,
                lon: 5.0,
                ele: Some(110.0),
            },
        ]);

        assert_eq!(summary.elevation_gain_m, 10.0);
        assert_eq!(summary.elevation_loss_m, 0.0);
    }

    #[test]
    fn counts_elevation_after_direction_change_exceeds_deadband() {
        let summary = summarize_route(vec![
            RoutePoint {
                lat: 45.0,
                lon: 5.0,
                ele: Some(100.0),
            },
            RoutePoint {
                lat: 45.001,
                lon: 5.0,
                ele: Some(110.0),
            },
            RoutePoint {
                lat: 45.002,
                lon: 5.0,
                ele: Some(108.0),
            },
            RoutePoint {
                lat: 45.003,
                lon: 5.0,
                ele: Some(95.0),
            },
        ]);

        assert_eq!(summary.elevation_gain_m, 10.0);
        assert_eq!(summary.elevation_loss_m, 15.0);
    }

    #[ignore = "long fixture regression; run with --ignored --nocapture"]
    #[test]
    fn very_long_trace_elevation_stays_between_reference_apps() {
        let summary = parse_gpx_route(include_bytes!("../tests/fixtures/very_long_trace.gpx"))
            .expect("very long trace fixture should parse");

        eprintln!(
            "distance={:.0}km gain={:.0}m loss={:.0}m",
            summary.distance_m / 1000.0,
            summary.elevation_gain_m,
            summary.elevation_loss_m
        );
        assert!((summary.distance_m / 1000.0 - 1625.0).abs() < 5.0);
        assert!(summary.elevation_gain_m > 9_634.0);
        assert!(summary.elevation_gain_m < 11_725.0);
        assert!(summary.elevation_loss_m > 9_415.0);
        assert!(summary.elevation_loss_m < 11_506.0);
    }

    #[ignore = "long fixture regression; run with --ignored --nocapture"]
    #[test]
    fn chartreuse_elevation_stays_between_reference_apps() {
        let summary = parse_gpx_route(include_bytes!("../tests/fixtures/Tour_Chartreuse.gpx"))
            .expect("Chartreuse fixture should parse");

        eprintln!(
            "distance={:.1}km gain={:.0}m loss={:.0}m",
            summary.distance_m / 1000.0,
            summary.elevation_gain_m,
            summary.elevation_loss_m
        );
        assert!((summary.distance_m / 1000.0 - 86.6).abs() < 1.0);
        assert!(summary.elevation_gain_m > 1_996.0);
        assert!(summary.elevation_gain_m < 2_244.0);
        assert!(summary.elevation_loss_m > 1_995.0);
        assert!(summary.elevation_loss_m < 2_245.0);
    }

    #[test]
    fn accepts_compegps_trace_with_empty_license_metadata() {
        let fixture = include_bytes!("../tests/fixtures/compegps_empty_license_anonymized.gpx");
        assert!(matches!(
            gpx::read(Cursor::new(fixture)),
            Err(GpxError::NoStringContent)
        ));

        let summary = parse_gpx_route(fixture).expect("anonymized CompeGPS trace should parse");

        assert_eq!(summary.points.len(), 947);
    }

    #[test]
    fn removes_only_empty_license_elements() {
        assert_eq!(
            remove_empty_license_elements(b"<metadata><license /></metadata>").unwrap(),
            b"<metadata></metadata>"
        );
        assert_eq!(
            remove_empty_license_elements(b"<license>\r\n  </license>").unwrap(),
            b""
        );
        assert!(remove_empty_license_elements(b"<license>MIT</license>").is_none());
        assert!(remove_empty_license_elements(b"<licensee></licensee>").is_none());
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
