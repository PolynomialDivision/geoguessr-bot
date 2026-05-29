//! Static list of world countries used for:
//!  - seed locations for the Wikimedia geosearch source
//!  - distractor options in the multiple-choice answer list

use rand::seq::SliceRandom;

pub struct Country {
    pub name:   &'static str,
    pub region: &'static str,
    /// Approximate capital / centre coordinates for Wikimedia geosearch.
    pub lat:    f64,
    pub lon:    f64,
    /// ISO 3166-1 alpha-2 code (used for country filter in config).
    pub iso:    &'static str,
}

pub const COUNTRIES: &[Country] = &[
    // Europe
    Country { name: "Germany",        region: "Europe",       lat:  52.52, lon:  13.40, iso: "DE" },
    Country { name: "France",         region: "Europe",       lat:  48.85, lon:   2.35, iso: "FR" },
    Country { name: "United Kingdom", region: "Europe",       lat:  51.51, lon:  -0.13, iso: "GB" },
    Country { name: "Italy",          region: "Europe",       lat:  41.90, lon:  12.49, iso: "IT" },
    Country { name: "Spain",          region: "Europe",       lat:  40.42, lon:  -3.70, iso: "ES" },
    Country { name: "Netherlands",    region: "Europe",       lat:  52.37, lon:   4.90, iso: "NL" },
    Country { name: "Sweden",         region: "Europe",       lat:  59.33, lon:  18.07, iso: "SE" },
    Country { name: "Norway",         region: "Europe",       lat:  59.91, lon:  10.75, iso: "NO" },
    Country { name: "Denmark",        region: "Europe",       lat:  55.68, lon:  12.57, iso: "DK" },
    Country { name: "Switzerland",    region: "Europe",       lat:  46.95, lon:   7.45, iso: "CH" },
    Country { name: "Austria",        region: "Europe",       lat:  48.21, lon:  16.37, iso: "AT" },
    Country { name: "Belgium",        region: "Europe",       lat:  50.85, lon:   4.35, iso: "BE" },
    Country { name: "Portugal",       region: "Europe",       lat:  38.72, lon:  -9.14, iso: "PT" },
    Country { name: "Poland",         region: "Europe",       lat:  52.23, lon:  21.01, iso: "PL" },
    Country { name: "Czech Republic", region: "Europe",       lat:  50.08, lon:  14.47, iso: "CZ" },
    Country { name: "Hungary",        region: "Europe",       lat:  47.50, lon:  19.04, iso: "HU" },
    Country { name: "Romania",        region: "Europe",       lat:  44.43, lon:  26.10, iso: "RO" },
    Country { name: "Greece",         region: "Europe",       lat:  37.98, lon:  23.73, iso: "GR" },
    Country { name: "Finland",        region: "Europe",       lat:  60.17, lon:  24.93, iso: "FI" },
    Country { name: "Iceland",        region: "Europe",       lat:  64.14, lon: -21.94, iso: "IS" },
    Country { name: "Ireland",        region: "Europe",       lat:  53.34, lon:  -6.27, iso: "IE" },
    Country { name: "Croatia",        region: "Europe",       lat:  45.81, lon:  15.98, iso: "HR" },
    Country { name: "Bulgaria",       region: "Europe",       lat:  42.70, lon:  23.32, iso: "BG" },
    Country { name: "Serbia",         region: "Europe",       lat:  44.80, lon:  20.46, iso: "RS" },
    Country { name: "Slovakia",       region: "Europe",       lat:  48.15, lon:  17.11, iso: "SK" },

    // Americas
    Country { name: "United States",  region: "Americas",     lat:  38.91, lon: -77.04, iso: "US" },
    Country { name: "Canada",         region: "Americas",     lat:  45.42, lon: -75.70, iso: "CA" },
    Country { name: "Brazil",         region: "Americas",     lat: -15.78, lon: -47.93, iso: "BR" },
    Country { name: "Argentina",      region: "Americas",     lat: -34.61, lon: -58.38, iso: "AR" },
    Country { name: "Mexico",         region: "Americas",     lat:  19.43, lon: -99.13, iso: "MX" },
    Country { name: "Colombia",       region: "Americas",     lat:   4.71, lon: -74.07, iso: "CO" },
    Country { name: "Chile",          region: "Americas",     lat: -33.46, lon: -70.65, iso: "CL" },
    Country { name: "Peru",           region: "Americas",     lat: -12.05, lon: -77.05, iso: "PE" },
    Country { name: "Cuba",           region: "Americas",     lat:  23.13, lon: -82.38, iso: "CU" },

    // Asia
    Country { name: "Japan",          region: "Asia",         lat:  35.69, lon: 139.69, iso: "JP" },
    Country { name: "China",          region: "Asia",         lat:  39.91, lon: 116.39, iso: "CN" },
    Country { name: "India",          region: "Asia",         lat:  28.61, lon:  77.21, iso: "IN" },
    Country { name: "South Korea",    region: "Asia",         lat:  37.57, lon: 126.98, iso: "KR" },
    Country { name: "Thailand",       region: "Asia",         lat:  13.75, lon: 100.52, iso: "TH" },
    Country { name: "Vietnam",        region: "Asia",         lat:  21.03, lon: 105.83, iso: "VN" },
    Country { name: "Indonesia",      region: "Asia",         lat:  -6.21, lon: 106.85, iso: "ID" },
    Country { name: "Malaysia",       region: "Asia",         lat:   3.14, lon: 101.69, iso: "MY" },
    Country { name: "Singapore",      region: "Asia",         lat:   1.36, lon: 103.82, iso: "SG" },
    Country { name: "Philippines",    region: "Asia",         lat:  14.60, lon: 120.98, iso: "PH" },
    Country { name: "Nepal",          region: "Asia",         lat:  27.70, lon:  85.31, iso: "NP" },
    Country { name: "Sri Lanka",      region: "Asia",         lat:   6.93, lon:  79.85, iso: "LK" },
    Country { name: "Taiwan",         region: "Asia",         lat:  25.05, lon: 121.52, iso: "TW" },
    Country { name: "Turkey",         region: "Asia",         lat:  39.93, lon:  32.86, iso: "TR" },
    Country { name: "Pakistan",       region: "Asia",         lat:  33.72, lon:  73.06, iso: "PK" },
    Country { name: "Bangladesh",     region: "Asia",         lat:  23.73, lon:  90.39, iso: "BD" },
    Country { name: "Kazakhstan",     region: "Asia",         lat:  51.16, lon:  71.45, iso: "KZ" },
    Country { name: "Georgia",        region: "Asia",         lat:  41.69, lon:  44.83, iso: "GE" },
    Country { name: "Armenia",        region: "Asia",         lat:  40.18, lon:  44.51, iso: "AM" },

    // Middle East
    Country { name: "Israel",         region: "Middle East",  lat:  31.77, lon:  35.22, iso: "IL" },
    Country { name: "Jordan",         region: "Middle East",  lat:  31.95, lon:  35.93, iso: "JO" },
    Country { name: "United Arab Emirates", region: "Middle East", lat: 24.45, lon: 54.38, iso: "AE" },
    Country { name: "Saudi Arabia",   region: "Middle East",  lat:  24.69, lon:  46.72, iso: "SA" },
    Country { name: "Iran",           region: "Middle East",  lat:  35.69, lon:  51.42, iso: "IR" },

    // Africa
    Country { name: "South Africa",   region: "Africa",       lat: -25.75, lon:  28.19, iso: "ZA" },
    Country { name: "Egypt",          region: "Africa",       lat:  30.06, lon:  31.25, iso: "EG" },
    Country { name: "Kenya",          region: "Africa",       lat:  -1.29, lon:  36.82, iso: "KE" },
    Country { name: "Morocco",        region: "Africa",       lat:  34.01, lon:  -6.85, iso: "MA" },
    Country { name: "Tanzania",       region: "Africa",       lat:  -6.17, lon:  35.74, iso: "TZ" },
    Country { name: "Ethiopia",       region: "Africa",       lat:   9.03, lon:  38.74, iso: "ET" },
    Country { name: "Ghana",          region: "Africa",       lat:   5.56, lon:  -0.20, iso: "GH" },
    Country { name: "Nigeria",        region: "Africa",       lat:   9.07, lon:   7.40, iso: "NG" },
    Country { name: "Tunisia",        region: "Africa",       lat:  36.82, lon:  10.16, iso: "TN" },
    Country { name: "Senegal",        region: "Africa",       lat:  14.74, lon: -17.47, iso: "SN" },
    Country { name: "Uganda",         region: "Africa",       lat:   0.32, lon:  32.58, iso: "UG" },

    // Oceania
    Country { name: "Australia",      region: "Oceania",      lat: -35.28, lon: 149.13, iso: "AU" },
    Country { name: "New Zealand",    region: "Oceania",      lat: -41.29, lon: 174.78, iso: "NZ" },
    Country { name: "Fiji",           region: "Oceania",      lat: -18.14, lon: 178.44, iso: "FJ" },
];

/// Return all country names as a flat list.
#[allow(dead_code)]
pub fn all_names() -> Vec<&'static str> {
    COUNTRIES.iter().map(|c| c.name).collect()
}

/// Pick `n` distractor country names, preferring the same region as `correct`
/// (to make the game harder) but filling from other regions if needed.
/// The correct answer is excluded from the result.
#[allow(dead_code)]
pub fn pick_distractors(correct: &str, correct_region: &str, n: usize) -> Vec<String> {
    let mut rng = rand::thread_rng();

    let same_region: Vec<&str> = COUNTRIES
        .iter()
        .filter(|c| c.region == correct_region && c.name != correct)
        .map(|c| c.name)
        .collect();

    let other: Vec<&str> = COUNTRIES
        .iter()
        .filter(|c| c.region != correct_region && c.name != correct)
        .map(|c| c.name)
        .collect();

    let mut pool: Vec<&str> = same_region;
    pool.shuffle(&mut rng);

    let mut extra: Vec<&str> = other;
    extra.shuffle(&mut rng);
    pool.extend(extra);

    pool.truncate(n);
    pool.iter().map(|s| s.to_string()).collect()
}

/// Look up a country by ISO code.
#[allow(dead_code)]
pub fn by_iso(iso: &str) -> Option<&'static Country> {
    COUNTRIES.iter().find(|c| c.iso.eq_ignore_ascii_case(iso))
}
