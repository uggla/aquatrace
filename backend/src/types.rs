use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RoutePoint {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ele: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteSummary {
    pub distance_m: f64,
    pub elevation_gain_m: f64,
    pub elevation_loss_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_elevation_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_elevation_m: Option<f64>,
    pub points: Vec<RoutePoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WaterPointResult {
    pub osm_id: i64,
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub km: f64,
    pub distance_to_route_m: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalyzeResponse {
    pub route: RouteSummary,
    pub water_points: Vec<WaterPointResult>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

impl BBox {
    pub fn from_points(points: &[RoutePoint]) -> Option<Self> {
        let first = points.first()?;
        let mut bbox = Self {
            min_lat: first.lat,
            min_lon: first.lon,
            max_lat: first.lat,
            max_lon: first.lon,
        };

        for point in &points[1..] {
            bbox.min_lat = bbox.min_lat.min(point.lat);
            bbox.min_lon = bbox.min_lon.min(point.lon);
            bbox.max_lat = bbox.max_lat.max(point.lat);
            bbox.max_lon = bbox.max_lon.max(point.lon);
        }

        Some(bbox)
    }

    pub fn expand_meters(self, meters: f64) -> Self {
        let lat_delta = meters / 111_320.0;
        let center_lat = ((self.min_lat + self.max_lat) / 2.0).to_radians();
        let lon_meters = (111_320.0 * center_lat.cos().abs()).max(1.0);
        let lon_delta = meters / lon_meters;

        Self {
            min_lat: self.min_lat - lat_delta,
            min_lon: self.min_lon - lon_delta,
            max_lat: self.max_lat + lat_delta,
            max_lon: self.max_lon + lon_delta,
        }
    }

    pub fn contains(self, other: Self) -> bool {
        self.min_lat <= other.min_lat
            && self.min_lon <= other.min_lon
            && self.max_lat >= other.max_lat
            && self.max_lon >= other.max_lon
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OsmWaterPoint {
    pub osm_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub name: Option<String>,
}
