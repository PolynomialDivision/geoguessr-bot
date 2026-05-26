//! Reverse geocoding via Nominatim (OpenStreetMap).
//!
//! Returns a human-readable place name for a lat/lon pair, used to label
//! player guesses in the results message.

use serde::Deserialize;

const NOMINATIM_URL: &str = "https://nominatim.openstreetmap.org/reverse";
const USER_AGENT: &str    = concat!("geoguessr-bot/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct NominatimResponse {
    address: NominatimAddress,
}

#[derive(Debug, Deserialize, Default)]
struct NominatimAddress {
    city:         Option<String>,
    town:         Option<String>,
    village:      Option<String>,
    municipality: Option<String>,
    county:       Option<String>,
    state:        Option<String>,
    country:      Option<String>,
}

impl NominatimAddress {
    /// Most specific populated place available.
    fn locality(&self) -> Option<&str> {
        self.city
            .as_deref()
            .or(self.town.as_deref())
            .or(self.village.as_deref())
            .or(self.municipality.as_deref())
            .or(self.county.as_deref())
            .or(self.state.as_deref())
    }
}

/// Reverse-geocode `(lat, lon)` and return a short label like
/// `"Astana, Kazakhstan"` or just `"Kazakhstan"` if no city is found.
/// Returns `None` on network/parse failure; callers should fall back to
/// showing the raw coordinates.
pub async fn reverse_geocode(lat: f64, lon: f64) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let url = format!(
        "{NOMINATIM_URL}?lat={lat}&lon={lon}&format=json&zoom=10",
    );
    let resp: NominatimResponse = client
        .get(&url)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    let addr = &resp.address;
    Some(match (addr.locality(), addr.country.as_deref()) {
        (Some(loc), Some(country)) => format!("{loc}, {country}"),
        (Some(loc), None)          => loc.to_owned(),
        (None,      Some(country)) => country.to_owned(),
        (None,      None)          => return None,
    })
}
