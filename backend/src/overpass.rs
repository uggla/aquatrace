use anyhow::{Context, Result};
use reqwest::{
    header::{ACCEPT, USER_AGENT},
    Client,
};
use serde::Deserialize;

use crate::types::{BBox, OsmWaterPoint};

#[derive(Clone)]
pub struct OverpassClient {
    client: Client,
    url: String,
}

impl OverpassClient {
    pub fn new(url: String) -> Self {
        let client = Client::builder()
            .user_agent("AquaTrace/0.1")
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            url,
        }
    }

    pub async fn fetch_drinking_water(&self, bbox: BBox) -> Result<Vec<OsmWaterPoint>> {
        let query = drinking_water_query(bbox);
        let response = self
            .client
            .post(&self.url)
            .header(USER_AGENT, "AquaTrace/0.1")
            .header(ACCEPT, "application/json")
            .form(&[("data", query)])
            .send()
            .await
            .context("failed to call Overpass API")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Overpass response")?;

        if !status.is_success() {
            let preview: String = body.chars().take(500).collect();
            anyhow::bail!("Overpass API returned {status}: {preview}");
        }

        parse_overpass_response(&body)
    }
}

pub fn drinking_water_query(bbox: BBox) -> String {
    format!(
        r#"[out:json][timeout:25];
node["amenity"="drinking_water"]({},{},{},{});
out body;"#,
        bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
    )
}

pub fn parse_overpass_response(body: &str) -> Result<Vec<OsmWaterPoint>> {
    let response: OverpassResponse =
        serde_json::from_str(body).context("failed to parse Overpass JSON")?;

    Ok(response
        .elements
        .into_iter()
        .filter_map(|element| {
            if element.kind != "node" {
                return None;
            }

            let tags = element.tags?;
            if tags.amenity.as_deref() != Some("drinking_water") {
                return None;
            }

            Some(OsmWaterPoint {
                osm_id: element.id,
                lat: element.lat?,
                lon: element.lon?,
                name: tags.name,
            })
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct OverpassResponse {
    elements: Vec<OverpassElement>,
}

#[derive(Debug, Deserialize)]
struct OverpassElement {
    #[serde(rename = "type")]
    kind: String,
    id: i64,
    lat: Option<f64>,
    lon: Option<f64>,
    tags: Option<OverpassTags>,
}

#[derive(Debug, Deserialize)]
struct OverpassTags {
    amenity: Option<String>,
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_only_explicit_drinking_water_nodes() {
        let body = r#"{
          "elements": [
            {"type":"node","id":1,"lat":45.0,"lon":5.0,"tags":{"amenity":"drinking_water","name":"Tap"}},
            {"type":"node","id":2,"lat":45.1,"lon":5.1,"tags":{"natural":"spring"}},
            {"type":"node","id":3,"lat":45.2,"lon":5.2,"tags":{"amenity":"fountain"}},
            {"type":"way","id":4,"tags":{"amenity":"drinking_water"}}
          ]
        }"#;

        let points = parse_overpass_response(body).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].osm_id, 1);
        assert_eq!(points[0].name.as_deref(), Some("Tap"));
    }
}
