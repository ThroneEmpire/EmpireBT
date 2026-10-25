//! Peers' countries, from DB-IP's free "IP to Country Lite" database (the
//! one qBittorrent uses), licensed CC BY 4.0: interfaces showing countries
//! credit "IP geolocation by DB-IP" (<https://db-ip.com>).
//!
//! The database is a gzipped CSV of address ranges, published monthly. It
//! is downloaded into the data directory on first use and again once it is
//! more than a month old, in the background and through the chosen network
//! interface; until it is ready, peers simply have no country.
//!
//! The ranges cover the address space back to back, so only each range's
//! start and country are kept: about 9 MB for some 700,000 ranges, searched
//! by bisection.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// The file kept in the data directory.
pub(crate) const FILE: &str = "ip-country.csv.gz";
/// Downloaded again when older than this.
const MAX_AGE: Duration = Duration::from_secs(32 * 24 * 60 * 60);
/// The largest download accepted; the file is about 5 MB.
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// What DB-IP asks interfaces to show.
pub const ATTRIBUTION: &str = "IP geolocation by DB-IP (db-ip.com)";
/// Marks addresses with no country (reserved, unassigned).
const NONE: [u8; 2] = *b"ZZ";

/// Address ranges and their countries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Countries {
    /// Range starts, ascending, with ISO 3166 country codes.
    v4: Vec<(u32, [u8; 2])>,
    v6: Vec<(u128, [u8; 2])>,
}

impl Countries {
    /// Reads DB-IP's CSV: `start,end,country` per line. Gaps between ranges
    /// have no country.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut countries = Self::default();
        let mut next_v4: Option<u32> = Some(0);
        let mut next_v6: Option<u128> = Some(0);
        for (number, line) in text.lines().enumerate() {
            let mut fields = line.trim().split(',');
            let (Some(start), Some(end), Some(code)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let code = code.trim_matches('"').as_bytes();
            let code: [u8; 2] = match code {
                [a, b] if a.is_ascii_uppercase() && b.is_ascii_uppercase() => [*a, *b],
                _ => NONE,
            };
            let bad = || format!("line {}: bad address range", number + 1);
            match (start.parse::<IpAddr>(), end.parse::<IpAddr>()) {
                (Ok(IpAddr::V4(start)), Ok(IpAddr::V4(end))) => {
                    let (start, end) = (u32::from(start), u32::from(end));
                    if end < start {
                        return Err(bad());
                    }
                    if next_v4.is_some_and(|next| start > next) {
                        countries.v4.push((next_v4.unwrap_or(0), NONE));
                    }
                    countries.v4.push((start, code));
                    next_v4 = end.checked_add(1);
                }
                (Ok(IpAddr::V6(start)), Ok(IpAddr::V6(end))) => {
                    let (start, end) = (u128::from(start), u128::from(end));
                    if end < start {
                        return Err(bad());
                    }
                    if next_v6.is_some_and(|next| start > next) {
                        countries.v6.push((next_v6.unwrap_or(0), NONE));
                    }
                    countries.v6.push((start, code));
                    next_v6 = end.checked_add(1);
                }
                _ => return Err(bad()),
            }
        }
        if let Some(next) = next_v4
            && !countries.v4.is_empty()
        {
            countries.v4.push((next, NONE));
        }
        if let Some(next) = next_v6
            && !countries.v6.is_empty()
        {
            countries.v6.push((next, NONE));
        }
        // Published sorted; sorting anyway makes bisection safe.
        countries.v4.sort_unstable_by_key(|(start, _)| *start);
        countries.v6.sort_unstable_by_key(|(start, _)| *start);
        if countries.v4.is_empty() && countries.v6.is_empty() {
            return Err("no address ranges".to_owned());
        }
        Ok(countries)
    }

    /// The country of an address, as an ISO 3166 code such as `DE`.
    pub fn lookup(&self, ip: IpAddr) -> Option<[u8; 2]> {
        let code = match ip {
            IpAddr::V4(ip) => find(&self.v4, u32::from(ip)),
            IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
                Some(v4) => find(&self.v4, u32::from(v4)),
                None => find(&self.v6, u128::from(ip)),
            },
        }?;
        (code != NONE).then_some(code)
    }

    /// Reads a gzipped CSV file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let compressed =
            std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        Self::parse(&gunzip(&compressed)?)
    }
}

fn find<T: Ord + Copy>(ranges: &[(T, [u8; 2])], address: T) -> Option<[u8; 2]> {
    let index = ranges.partition_point(|(start, _)| *start <= address);
    index.checked_sub(1).map(|index| ranges[index].1)
}

fn gunzip(compressed: &[u8]) -> Result<String, String> {
    use std::io::Read;
    let mut text = String::new();
    flate2::read::GzDecoder::new(compressed)
        .take(256 * 1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|error| format!("not a gzipped CSV file: {error}"))?;
    Ok(text)
}

/// The database while it is loaded, shared with the engine.
#[derive(Debug, Clone, Default)]
pub(crate) struct GeoIp {
    current: Arc<RwLock<Option<Arc<Countries>>>>,
}

impl GeoIp {
    /// Loads the saved database, and downloads a fresh one when it is
    /// missing or old, on a background thread.
    pub fn start(path: PathBuf, binding: empirebt_backend::Binding) -> Self {
        let geoip = Self::default();
        let current = Arc::clone(&geoip.current);
        let _ = std::thread::Builder::new()
            .name("geoip".to_owned())
            .spawn(move || {
                let set = |countries: Countries| {
                    if let Ok(mut slot) = current.write() {
                        *slot = Some(Arc::new(countries));
                    }
                };
                if let Ok(countries) = Countries::load(&path) {
                    set(countries);
                }
                let age = std::fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                if age.is_some_and(|age| age < MAX_AGE) {
                    return;
                }
                if let Ok(countries) = refresh(&path, &binding) {
                    set(countries);
                }
            });
        geoip
    }

    pub fn lookup(&self, ip: IpAddr) -> Option<[u8; 2]> {
        let countries = self.current.read().ok()?.clone()?;
        countries.lookup(ip)
    }
}

/// Downloads this month's database (or last month's, early in a month
/// before it is published), checks it, and saves it atomically.
fn refresh(path: &Path, binding: &empirebt_backend::Binding) -> Result<Countries, String> {
    let (year, month) = year_month(SystemTime::now());
    let previous = if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    };
    let mut last_error = String::new();
    for (year, month) in [(year, month), previous] {
        let url =
            format!("https://download.db-ip.com/free/dbip-country-lite-{year}-{month:02}.csv.gz");
        match empirebt_backend::download(&url, DOWNLOAD_TIMEOUT, MAX_DOWNLOAD, binding) {
            Ok(compressed) => {
                let countries = Countries::parse(&gunzip(&compressed)?)?;
                let temporary = path.with_extension("tmp");
                std::fs::write(&temporary, &compressed)
                    .and_then(|()| std::fs::rename(&temporary, path))
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                return Ok(countries);
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

/// The calendar year and month (1–12, UTC) of `time`.
fn year_month(time: SystemTime) -> (i64, u32) {
    let days = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() / 86_400) as i64;
    // Howard Hinnant's days-to-civil algorithm.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "0.0.0.0,0.255.255.255,ZZ\n\
1.0.0.0,1.0.0.255,AU\n\
1.0.1.0,1.0.3.255,CN\n\
5.0.0.0,5.255.255.255,DE\n\
2001:200::,2001:200:ffff:ffff:ffff:ffff:ffff:ffff,JP\n\
2a00::,2a00:ffff:ffff:ffff:ffff:ffff:ffff:ffff,NL\n";

    fn code(text: &str) -> Option<String> {
        Countries::parse(SAMPLE)
            .unwrap()
            .lookup(text.parse().unwrap())
            .map(|code| String::from_utf8_lossy(&code).into_owned())
    }

    #[test]
    fn finds_countries_by_address() {
        assert_eq!(code("1.0.0.7").as_deref(), Some("AU"));
        assert_eq!(code("1.0.2.200").as_deref(), Some("CN"));
        assert_eq!(code("5.9.0.1").as_deref(), Some("DE"));
        assert_eq!(code("2001:200::1").as_deref(), Some("JP"));
        assert_eq!(code("2a00:1450::1").as_deref(), Some("NL"));
        assert_eq!(code("::ffff:5.1.2.3").as_deref(), Some("DE"), "IPv4-mapped");
        // Reserved ranges, gaps between ranges, and past the last range.
        assert_eq!(code("0.1.2.3"), None);
        assert_eq!(code("2.0.0.1"), None);
        assert_eq!(code("200.1.1.1"), None);
        assert_eq!(code("2b00::1"), None);
        assert!(Countries::parse("nonsense\n").is_err());
        assert!(Countries::parse("1.0.0.0,::1,AU\n").is_err());
    }

    #[test]
    fn reads_gzipped_files() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("empirebt-geoip-{}.csv.gz", std::process::id()));
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(SAMPLE.as_bytes()).unwrap();
        std::fs::write(&path, encoder.finish().unwrap()).unwrap();
        let countries = Countries::load(&path).unwrap();
        assert_eq!(countries.lookup("1.0.0.1".parse().unwrap()), Some(*b"AU"));
        std::fs::write(&path, b"not gzip").unwrap();
        assert!(Countries::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    /// Needs network access: downloads this month's database the way the
    /// application does. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn downloads_and_reads_the_real_database() {
        let path =
            std::env::temp_dir().join(format!("empirebt-geoip-real-{}.csv.gz", std::process::id()));
        let started = std::time::Instant::now();
        let countries = refresh(&path, &empirebt_backend::Binding::any()).unwrap();
        eprintln!(
            "{} IPv4 and {} IPv6 ranges in {:?}",
            countries.v4.len(),
            countries.v6.len(),
            started.elapsed()
        );
        assert_eq!(countries.lookup("8.8.8.8".parse().unwrap()), Some(*b"US"));
        // DB-IP's own answers, whatever one might expect (it places some of
        // Google's IPv6 ranges in Canada).
        assert_eq!(countries.lookup("1.1.1.1".parse().unwrap()), Some(*b"AU"));
        assert!(
            countries
                .lookup("2001:4860:4860::8888".parse().unwrap())
                .is_some()
        );
        assert_eq!(
            countries.lookup("192.168.1.1".parse().unwrap()),
            None,
            "private"
        );
        assert!(Countries::load(&path).is_ok(), "saved");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn knows_the_month() {
        let at = |seconds: u64| year_month(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(at(0), (1970, 1));
        assert_eq!(at(951_782_400), (2000, 2), "2000-02-29");
        assert_eq!(at(1_793_404_800), (2026, 10), "2026-10-31");
        assert_eq!(at(1_793_491_200), (2026, 11), "2026-11-01");
        assert_eq!(at(1_798_761_600), (2027, 1), "2027-01-01");
    }
}
