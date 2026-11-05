//! finn_monitor_scraper
//!
//! Targeted FINN.no monitor hunter (Oslo / "Skjermer").
//!
//! Phase 1 - two precise searches (`q=34`, `q=ultrawide`), 3 pages each,
//!           merged and de-duplicated by FINN-kode.
//! Phase 2 - hard in-memory title filter (blacklist + size/keyword whitelist).
//! Phase 3 - deep-scrape of every qualified ad: condition, exact place and the
//!           full seller description.
//! Phase 4 - write `monitory_34_oslo_pelne.txt`.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rand::Rng;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use scraper::{ElementRef, Html, Selector};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const SEARCH_BASE: &str = "https://www.finn.no/recommerce/forsale/search";
const ITEM_BASE: &str = "https://www.finn.no/recommerce/forsale/item";

const LOCATION: &str = "0.20061"; // Oslo
const SUB_CATEGORY: &str = "1.93.3215"; // Skjermer
const SORT: &str = "PUBLISHED_DESC"; // Nyeste først

/// Targeted search terms (STEP 1).
const QUERIES: &[&str] = &["34", "ultrawide"];
/// Pages walked per query (STEP 1).
const PAGES_PER_QUERY: u32 = 3;

const OUTPUT_FILE: &str = "monitory_34_oslo_pelne.txt";

/// Anti-ban jitter between search requests (STEP 1).
const SEARCH_JITTER_MIN_MS: u64 = 2_200;
const SEARCH_JITTER_MAX_MS: u64 = 4_000;
/// Anti-ban jitter between detail/item requests (STEP 3).
const DETAIL_JITTER_MIN_MS: u64 = 1_500;
const DETAIL_JITTER_MAX_MS: u64 = 2_500;

/// Hard reject terms (STEP 2) - any match in the title drops the ad.
const REJECT_TERMS: &[&str] = &[
    "24\"", "25\"", "27\"", "macbook", "ipad", "laptop", "ram", "rtx", "gtx", "kabel", "arm",
    "feste", "tastatur", "mus",
];
/// Keep-list: a title must contain one of these sizes (STEP 2)...
const KEEP_SIZES: &[&str] = &["34", "35", "38", "40"];
/// ... and/or one of these keywords (STEP 2).
const KEEP_WORDS: &[&str] = &["ultrawide", "ultra-wide", "buet"];

const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const ACCEPT_VALUE: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
const ACCEPT_LANGUAGE_VALUE: &str = "nb,no;q=0.9,en-US;q=0.8,en;q=0.7";
const SEC_CH_UA_VALUE: &str = "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\"";
const SEC_CH_UA_PLATFORM_VALUE: &str = "\"Windows\"";

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A listing found on a search results page.
#[derive(Debug, Clone)]
struct Listing {
    id: String,
    title: String,
    price: String,
    location: String,
    url: String,
}

/// A fully deep-scraped listing (read from its own item page).
#[derive(Debug, Clone)]
struct Detail {
    id: String,
    title: String,
    price: String,
    condition: String,
    location: String,
    description: String,
    url: String,
}

/// Outcome of a single HTTP GET.
enum Fetch {
    /// Response body of a successful request.
    Body(String),
    /// A non-2xx HTTP status code.
    Status(u16),
    /// A transport-level error.
    Error(String),
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Błąd krytyczny: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let client = build_client()?;
    // Matches both the modern "/item/<id>" links and the legacy "finnkode=<id>".
    let finnkode_re = Regex::new(r"(?:finnkode=|/item/)(\d{4,})")?;
    let mut stopped_early = false;

    // ---- STEP 1: targeted search ------------------------------------------
    println!("== KROK 1: wyszukiwanie ukierunkowane (q=34, q=ultrawide) ==");
    let mut candidates: Vec<Listing> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut search_requests = 0u32;

    'search: for query in QUERIES {
        for page in 1..=PAGES_PER_QUERY {
            if search_requests > 0 {
                polite_pause(SEARCH_JITTER_MIN_MS, SEARCH_JITTER_MAX_MS).await;
            }
            search_requests += 1;

            let url = format!(
                "{SEARCH_BASE}?location={LOCATION}&sub_category={SUB_CATEGORY}&q={query}&sort={SORT}&page={page}"
            );
            println!("-> q={query} strona {page}/{PAGES_PER_QUERY}");

            match fetch(&client, &url).await {
                Fetch::Body(body) => {
                    let found = parse_search_page(&body, &finnkode_re);
                    let total = found.len();
                    let mut fresh = 0usize;
                    for listing in found {
                        if seen.insert(listing.id.clone()) {
                            candidates.push(listing);
                            fresh += 1;
                        }
                    }
                    println!(
                        "   . {total} kart, {fresh} nowych (łącznie {})",
                        candidates.len()
                    );
                }
                Fetch::Status(code) => {
                    eprintln!("   ! HTTP {code} — przerywam wyszukiwanie.");
                    stopped_early = true;
                    break 'search;
                }
                Fetch::Error(err) => {
                    eprintln!("   ! Błąd sieci: {err} — przerywam wyszukiwanie.");
                    stopped_early = true;
                    break 'search;
                }
            }
        }
    }

    println!("   . Po scaleniu/de-duplikacji: {} ogłoszeń", candidates.len());

    // ---- STEP 2: hard in-memory filter ------------------------------------
    println!("== KROK 2: filtr twardy w pamięci ==");
    let before = candidates.len();
    candidates.retain(|listing| is_candidate(&listing.title));
    println!(
        "   . {before} -> {} zakwalifikowanych monitorów",
        candidates.len()
    );

    if candidates.is_empty() {
        eprintln!("Brak zakwalifikowanych monitorów — plik nie został zapisany.");
        return Ok(());
    }

    // ---- STEP 3: deep-scrape ----------------------------------------------
    println!("== KROK 3: deep-scraping pełnych opisów ==");
    let mut details: Vec<Detail> = Vec::new();

    'details: for (index, listing) in candidates.iter().enumerate() {
        if index > 0 {
            polite_pause(DETAIL_JITTER_MIN_MS, DETAIL_JITTER_MAX_MS).await;
        }

        let url = format!("{ITEM_BASE}/{}", listing.id);
        println!("-> {} — {}", listing.id, truncate(&listing.title, 70));

        match fetch(&client, &url).await {
            Fetch::Body(body) => details.push(parse_item_page(&body, listing)),
            Fetch::Status(code) if code == 403 || code == 429 => {
                eprintln!("   ! HTTP {code} (anti-bot) — przerywam deep-scraping.");
                stopped_early = true;
                break 'details;
            }
            Fetch::Status(code) => eprintln!("   ! Pomijam {} (HTTP {code}).", listing.id),
            Fetch::Error(err) => eprintln!("   ! Pomijam {} (błąd: {err}).", listing.id),
        }
    }

    // ---- STEP 4: write -----------------------------------------------------
    println!("== KROK 4: zapis do pliku ==");
    if details.is_empty() {
        eprintln!("Nie pobrano żadnych opisów — plik nie został zapisany.");
        return Ok(());
    }

    let path = write_output(&details)?;

    println!();
    println!("Podsumowanie:");
    println!("  · Ogłoszenia po de-duplikacji : {}", before);
    println!("  · Zakwalifikowane monitory    : {}", candidates.len());
    println!("  · Pobrane pełne opisy         : {}", details.len());
    println!("  · Zapisano plik               : {}", path.display());
    if stopped_early {
        println!("  · Uwaga: zbieranie przerwane wcześniej (limit / anti-bot / błąd sieci).");
    }

    Ok(())
}


// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

/// Performs a GET request and classifies the outcome for the caller.
async fn fetch(client: &reqwest::Client, url: &str) -> Fetch {
    match client.get(url).send().await {
        Ok(response) => {
            if !response.status().is_success() {
                return Fetch::Status(response.status().as_u16());
            }
            match response.text().await {
                Ok(body) => Fetch::Body(body),
                Err(err) => Fetch::Error(err.to_string()),
            }
        }
        Err(err) => Fetch::Error(err.to_string()),
    }
}

/// Builds the stealth `reqwest` client: gzip decompression + browser headers.
fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(ACCEPT, HeaderValue::from_static(ACCEPT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(
        HeaderName::from_static("sec-ch-ua"),
        HeaderValue::from_static(SEC_CH_UA_VALUE),
    );
    headers.insert(
        HeaderName::from_static("sec-ch-ua-mobile"),
        HeaderValue::from_static("?0"),
    );
    headers.insert(
        HeaderName::from_static("sec-ch-ua-platform"),
        HeaderValue::from_static(SEC_CH_UA_PLATFORM_VALUE),
    );
    headers.insert(
        HeaderName::from_static("upgrade-insecure-requests"),
        HeaderValue::from_static("1"),
    );

    reqwest::Client::builder()
        .gzip(true)
        .timeout(Duration::from_secs(30))
        .default_headers(headers)
        .build()
}

/// Sleeps for a random duration in the inclusive `[min_ms, max_ms]` range.
async fn polite_pause(min_ms: u64, max_ms: u64) {
    let delay_ms = {
        let mut rng = rand::thread_rng();
        rng.gen_range(min_ms..=max_ms)
    };
    println!("   . Odczekuję {delay_ms} ms (jitter anti-ban)...");
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

/// STEP 2: hard title filter (blacklist first, then size/keyword whitelist).
fn is_candidate(title: &str) -> bool {
    let lower = title.to_lowercase();
    if REJECT_TERMS.iter().any(|term| lower.contains(*term)) {
        return false;
    }
    KEEP_SIZES.iter().any(|size| lower.contains(*size))
        || KEEP_WORDS.iter().any(|word| lower.contains(*word))
}

/// Shortens `text` to at most `max_chars` characters for console output.
fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

// ---------------------------------------------------------------------------
// Small parsing utilities
// ---------------------------------------------------------------------------

/// Builds a `Selector`; panics on an invalid literal (a programmer error only).
fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("valid CSS selector")
}

/// Returns the first element's normalized text for `selector` inside `scope`.
fn first_text(scope: &ElementRef, selector: &Selector) -> Option<String> {
    scope
        .select(selector)
        .next()
        .map(|el| clean(&el.text().collect::<String>()))
        .filter(|s| !s.is_empty())
}

/// Normalized, whitespace-collapsed plain text of an element.
fn article_text(scope: &ElementRef) -> String {
    clean(&scope.text().collect::<String>())
}

/// Collapses whitespace and converts non-breaking spaces into regular ones.
fn clean(input: &str) -> String {
    input
        .replace('\u{00a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalizes a scraped price; flags giveaways and missing prices.
fn format_price(raw: Option<&str>) -> String {
    let value = raw.map(clean).unwrap_or_default();
    if value.is_empty() {
        "BRAK CENY".to_string()
    } else if value.to_lowercase().contains("gis bort") {
        "Gis bort".to_string()
    } else {
        value
    }
}

/// Resolves an anchor href into an absolute URL.
fn absolute_url(href: &str, id: &str) -> String {
    if href.starts_with("http") {
        href.to_string()
    } else if let Some(rest) = href.strip_prefix('/') {
        format!("https://www.finn.no/{rest}")
    } else {
        format!("{ITEM_BASE}/{id}")
    }
}


// ---------------------------------------------------------------------------
// Page parsers
// ---------------------------------------------------------------------------

/// STEP 1: parses every result card on a search page into `Listing`s.
fn parse_search_page(html: &str, finnkode_re: &Regex) -> Vec<Listing> {
    let document = Html::parse_document(html);
    let article_sel = sel("article");
    let anchor_sel = sel("a[href]");
    let title_sel = sel("h2");
    let price_sel = sel("div.text-m.font-bold span");
    let location_sel = sel("div.s-text-subtle span");
    let price_re = Regex::new(r"\d[\d\u{00a0}\s]*\s*kr").expect("valid price regex");

    let mut result = Vec::new();

    for article in document.select(&article_sel) {
        // ID + URL from the first anchor pointing to an item page.
        let mut identity: Option<(String, String)> = None;
        for anchor in article.select(&anchor_sel) {
            let Some(href) = anchor.value().attr("href") else {
                continue;
            };
            if let Some(caps) = finnkode_re.captures(href) {
                let id = caps[1].to_string();
                let url = absolute_url(href, &id);
                identity = Some((id, url));
                break;
            }
        }
        let Some((id, url)) = identity else {
            continue; // skip ad pods / non-listing articles
        };

        let title = first_text(&article, &title_sel).unwrap_or_else(|| "(bez tytułu)".to_string());

        // Price: structured span first, then a textual fallback (e.g. "Gis bort").
        let raw_price = first_text(&article, &price_sel).or_else(|| {
            let text = article_text(&article);
            price_re
                .find(&text)
                .map(|m| m.as_str().to_string())
                .or_else(|| {
                    ["Gis bort", "Gi bud", "Auksjon", "Solgt", "Ønskes kjøpt"]
                        .iter()
                        .find(|kw| text.to_lowercase().contains(&kw.to_lowercase()))
                        .map(|kw| (*kw).to_string())
                })
        });
        let location = first_text(&article, &location_sel).unwrap_or_else(|| "Oslo".to_string());

        result.push(Listing {
            id,
            title,
            price: format_price(raw_price.as_deref()),
            location,
            url,
        });
    }

    result
}

/// STEP 3: reads the full detail of a single item page.
fn parse_item_page(html: &str, fallback: &Listing) -> Detail {
    let document = Html::parse_document(html);
    let root = document.root_element();

    let title_sel = sel("h1[data-testid=\"object-title\"]");
    let price_sel = sel("div.h2");
    let addr_sel = sel("[data-testid=\"object-address\"]");
    let keyinfo_sel = sel("section[aria-label=\"Nøkkelinfo\"] p");
    let desc_sel = sel("[data-testid=\"description\"]");
    let p_sel = sel("p");

    let title = first_text(&root, &title_sel).unwrap_or_else(|| fallback.title.clone());
    let price = first_text(&root, &price_sel).unwrap_or_else(|| fallback.price.clone());
    let location = first_text(&root, &addr_sel).unwrap_or_else(|| fallback.location.clone());

    // Condition ("Tilstand") from the "Nøkkelinfo" chips.
    let mut condition = String::from("Nie podano");
    for chip in document.select(&keyinfo_sel) {
        let text = clean(&chip.text().collect::<String>());
        if let Some(rest) = text.strip_prefix("Tilstand") {
            let value = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
            if !value.is_empty() {
                condition = value.to_string();
            }
            break;
        }
    }

    // Full seller description, preserving paragraph breaks.
    let description = document
        .select(&desc_sel)
        .next()
        .map(|section| {
            let paragraphs: Vec<String> = section
                .select(&p_sel)
                .map(|p| clean(&p.text().collect::<String>()))
                .filter(|s| !s.is_empty())
                .collect();
            if paragraphs.is_empty() {
                clean(&section.text().collect::<String>())
            } else {
                paragraphs.join("\n")
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(brak opisu)".to_string());

    Detail {
        id: fallback.id.clone(),
        title,
        price,
        condition,
        location,
        description,
        url: fallback.url.clone(),
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Formats a price for the output file (exactly one trailing " kr" for amounts).
fn price_for_output(price: &str) -> String {
    let value = clean(price);
    if value.is_empty() {
        return "brak ceny".to_string();
    }
    if !value.chars().any(|c| c.is_ascii_digit()) {
        return value; // e.g. "Gis bort", "Gi bud"
    }
    let trimmed = value.trim_end();
    let lower = trimmed.to_ascii_lowercase();
    let base = if lower.ends_with("kr") {
        trimmed[..trimmed.len() - 2].trim_end()
    } else if lower.ends_with("nok") {
        trimmed[..trimmed.len() - 3].trim_end()
    } else {
        trimmed
    };
    format!("{base} kr")
}

/// STEP 4: writes every detail to `OUTPUT_FILE` and returns its absolute path.
fn write_output(details: &[Detail]) -> Result<PathBuf, std::io::Error> {
    let separator = "=".repeat(60);
    let mut buffer = String::new();

    for detail in details {
        buffer.push_str(&separator);
        buffer.push('\n');
        buffer.push_str(&format!(
            "ID: {} | CENA: {} | STAN: {} | MIEJSCE: {}\n",
            detail.id,
            price_for_output(&detail.price),
            detail.condition,
            detail.location
        ));
        buffer.push_str(&format!("TYTUŁ: {}\n", detail.title));
        buffer.push_str(&format!("URL: {}\n", detail.url));
        buffer.push_str("--- OPIS SPRZEDAWCY ---\n");
        buffer.push_str(&detail.description);
        if !detail.description.ends_with('\n') {
            buffer.push('\n');
        }
        buffer.push_str(&separator);
        buffer.push('\n');
    }

    fs::write(OUTPUT_FILE, buffer.as_bytes())?;

    let absolute = std::env::current_dir()
        .map(|dir| dir.join(OUTPUT_FILE))
        .unwrap_or_else(|_| PathBuf::from(OUTPUT_FILE));
    Ok(absolute)
}


