mod rebrickable;

use crate::rebrickable::SETS;
use anyhow::{Context, Result, anyhow};
use axum::body::Body;
use axum::debug_handler;
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Router, extract::State, response::Html, routing::get};
use chrono::NaiveDateTime;
use clap::Parser;
use httpdate::fmt_http_date;
use pdf;
use percent_encoding::{NON_ALPHANUMERIC, PercentEncode, percent_decode_str, utf8_percent_encode};
use regex::Regex;
use sailfish::TemplateSimple;
use serde::{Deserialize, Serialize};
use serde_with::formats::CommaSeparator;
use serde_with::{DeserializeFromStr, NoneAsEmptyString, StringWithSeparator, serde_as};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::io::{BufReader, Read};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fmt, fs, io};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_util::io::ReaderStream;
use tower_http::services::ServeDir;
use xmp_toolkit::XmpMeta;
use xmp_toolkit::xmp_ns;
use zip::ZipArchive;

/// Serve cbz files from directory
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// Directory to serve
    #[arg(long)]
    dir: Option<String>,

    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,

    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    port: u16,
}

type SharedState = Arc<RwLock<AppState>>;

#[derive(Debug)]
struct AppState {
    sets: Vec<SetAggregate>,
    files: Vec<File>,
    all_years: BTreeSet<String>,
    all_genres: BTreeSet<String>,
}

#[derive(Debug)]
struct SetAggregate {
    info: SetInfo,
    files: Vec<String>,
}

impl SetAggregate {
    fn view_url(&self) -> String {
        if self.files.len() == 1 {
            let relative_path = &self.files[0];
            format!("/view/{}", encode_path_segment(relative_path),)
        } else {
            self.info.view_url()
        }
    }
}

impl AppState {
    fn from_files(mut files: Vec<File>) -> Self {
        let all_years = files
            .iter()
            .filter(|f| !f.year().is_empty())
            .map(|f| f.year().to_string())
            .collect::<BTreeSet<_>>();
        let all_genres = files
            .iter()
            .flat_map(|f| f.genres())
            .cloned()
            .collect::<BTreeSet<_>>();

        files.sort_by(|a, b| {
            split_name(a.number())
                .cmp(&split_name(b.number()))
                .then(a.title.cmp(&b.title))
        });

        let mut sets = HashMap::new();
        for file in &files {
            if let Some(info) = file.info.as_ref() {
                sets.entry(info.number.clone())
                    .or_insert_with(|| SetAggregate {
                        info: info.clone(),
                        files: vec![],
                    })
                    .files
                    .push(file.relative_path.to_owned())
            }
        }
        let mut sets = sets.into_values().collect::<Vec<_>>();
        sets.sort_by(|a, b| split_name(&a.info.number).cmp(&split_name(&b.info.number)));

        Self {
            sets,
            files,
            all_years,
            all_genres,
        }
    }

    fn get_set_by_number(&self, number: &str) -> Option<&SetInfo> {
        self.sets
            .iter()
            .find(|s| s.info.number == number)
            .map(|s| &s.info)
    }

    fn get_files_for_set(&self, set: &SetInfo) -> Vec<&File> {
        self.files
            .iter()
            .filter(|f| f.info.as_ref().is_some_and(|i| i.number == set.number))
            .collect::<Vec<_>>()
    }
}

#[derive(Debug, Serialize)]
struct File {
    title: String, // filename, or better if we can look up the right metadata
    relative_path: String,
    path: PathBuf,
    info: Option<SetInfo>,
    pages: usize,
    size: u64,
    modified: SystemTime,
    source: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct SetInfo {
    title: String,
    number: String,
    year: String,
    themes: Vec<String>,
}
impl SetInfo {
    fn name(&self) -> String {
        format!("{} {}", self.number, self.title)
    }

    fn from_xmp(xmp: &XmpMeta) -> Result<Self> {
        let title = xmp
            .localized_text(xmp_ns::DC, "title", Some("en"), "x-default")
            .context("missing dc:title")?
            .0
            .value;
        let number = xmp
            .property(xmp_ns::DC, "identifier")
            .context("missing dc:identifier")?
            .value;
        let date = xmp
            .property_array(xmp_ns::DC, "date")
            .next()
            .context("missing dc:date")?
            .value;
        let subject = xmp
            .property_array(xmp_ns::DC, "subject")
            .map(|s| s.value)
            .collect();

        Ok(Self {
            title,
            number,
            year: date,
            themes: subject,
        })
    }

    fn view_url(&self) -> String {
        format!("/sets/{}", encode_path_segment(self.number.as_str()),)
    }
}

impl From<ComicInfo> for SetInfo {
    fn from(value: ComicInfo) -> Self {
        Self {
            title: value.title,
            number: value.number,
            year: value.year,
            themes: value.genre,
        }
    }
}

fn theme_names(theme: &rebrickable::Theme) -> Vec<String> {
    theme
        .with_parents()
        .iter()
        .map(|t| t.name.to_owned())
        .collect()
}

impl From<&rebrickable::SetInfo> for SetInfo {
    fn from(value: &rebrickable::SetInfo) -> Self {
        Self {
            title: value.name.to_owned(),
            number: value.set_num.to_owned().into(),
            year: value.year.to_owned(),
            themes: theme_names(value.theme()),
        }
    }
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default)]
struct ComicInfo {
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Series")]
    series: String,
    #[serde(rename = "Number")]
    number: String,
    #[serde(rename = "Year")]
    year: String,
    #[serde(rename = "Publisher")]
    publisher: String,
    #[serde_as(as = "StringWithSeparator::<CommaSeparator, String>")]
    #[serde(rename = "Genre")]
    genre: Vec<String>,
    #[serde(rename = "Web")]
    web: String,
}

struct TitleAndInfo {
    title: String,
    source: Option<String>,
    set_info: Option<SetInfo>,
}

fn info_from_path(path: &Path) -> Result<TitleAndInfo> {
    // check for XMP sidecar
    let xmp_path = path.with_extension("xmp");
    if xmp_path.exists() {
        let xmp = XmpMeta::from_file(xmp_path)?;
        let info = SetInfo::from_xmp(&xmp)?;
        let source = xmp.property(xmp_ns::DC, "source").map(|v| v.value);

        return Ok(TitleAndInfo {
            title: info.title.clone(),
            source,
            set_info: Some(info),
        });
    }

    let filename = path.file_stem().unwrap().to_str().unwrap();
    Ok(info_from_filename(filename))
}

fn info_from_filename(filename: &str) -> TitleAndInfo {
    let mut filename = filename.to_owned();

    // "123%20Something.pdf"
    if filename.contains("%20") {
        if let Ok(decoded) = percent_decode_str(&filename).decode_utf8() {
            filename = decoded.to_string();
        }
    }

    // multi-book instructions use " (1)"; robertlee uses " [paged]"
    static SUFFIX_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"( \(\d+\)| \[.*?])+$").unwrap());
    let (filename, suffix) = if let Some(info) = SUFFIX_RE.find(&filename) {
        (filename[0..info.start()].to_owned(), info.as_str())
    } else {
        (filename, "")
    };

    // if it's an exact match, we're done
    // this is also needed to handle non-numeric ids like b55dk-01
    if let Some(set) = set_from_id(&filename, None) {
        return TitleAndInfo {
            title: set.name.to_owned() + suffix,
            source: None,
            set_info: Some(set.into()),
        };
    }

    static PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(?:-\d+)?\b").unwrap());
    if let Some(info) = PREFIX_RE.find(&filename) {
        let hint = filename[info.end()..].trim();
        // TODO: where does 5 come from?
        let hint = if hint.len() > 5 { Some(hint) } else { None };
        if let Some(set) = set_from_id(info.as_str(), hint) {
            return TitleAndInfo {
                title: set.name.to_owned() + suffix,
                source: None,
                set_info: Some(set.into()),
            };
        }
    }

    TitleAndInfo {
        title: filename + suffix,
        source: None,
        set_info: None,
    }
}

fn set_from_id<'a>(id: &'_ str, hint: Option<&'_ str>) -> Option<&'a rebrickable::SetInfo> {
    // exact match
    if let Some(info) = SETS.get(id) {
        return Some(info);
    }

    // "123" -> 123-1, but only if there's no 123-2
    if id.chars().all(|c| c.is_ascii_digit()) {
        let prefix = id.to_string() + "-";
        let options = SETS
            .iter()
            .filter(|(number, _)| number.starts_with(&prefix))
            .map(|(_, info)| info)
            .collect::<Vec<_>>();
        if options.len() < 2 {
            return options.into_iter().next();
        }

        // if we have more text from the filename, try to use that too
        if let Some(hint) = hint {
            let options = options
                .into_iter()
                // TODO: case folding, fuzzy match
                .filter(|info| info.name == hint)
                .collect::<Vec<_>>();
            if options.len() < 2 {
                return options.into_iter().next();
            }
        }
    }
    None
}

impl File {
    fn from_path(path: PathBuf, dir: &Path) -> Result<Self> {
        match path.extension().map(|e| e.to_str()).flatten() {
            Some("cbz") => Self::from_cbz(path, dir),
            Some("pdf") => Self::from_pdf(path, dir),
            _ => Err(anyhow!("Unsupported file extension")),
        }
    }

    fn from_cbz(path: PathBuf, dir: &Path) -> Result<Self> {
        let relative_path = path.strip_prefix(dir)?;
        let file = fs::File::open(&path)?;
        let metadata = file.metadata()?;

        let mut zip = ZipArchive::new(file)?;
        let pages = zip.file_names().filter(|f| should_expose(f)).count();
        let title_and_info = match zip.by_name("ComicInfo.xml") {
            Ok(info_xml) => {
                let info: ComicInfo = quick_xml::de::from_reader(BufReader::new(info_xml))?;
                TitleAndInfo {
                    title: info.title.clone(),
                    source: None,
                    set_info: Some(info.into()),
                }
            }
            _ => info_from_path(&path)?,
        };

        static SOURCE_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)^(?i:source):\s+(.*?)$").unwrap());
        let source_from_zip = match str::from_utf8(zip.comment()) {
            Ok(comment) => match SOURCE_RE.captures(comment) {
                Some(captures) => Some(captures.get(1).unwrap().as_str().to_owned()),
                _ => None,
            },
            _ => None,
        };

        Ok(Self {
            title: title_and_info.title,
            relative_path: relative_path.to_str().context("invalid path")?.to_owned(),
            path,
            info: title_and_info.set_info,
            pages,
            size: metadata.len(),
            modified: metadata.modified()?,
            source: title_and_info.source.or(source_from_zip),
        })
    }

    fn from_pdf(path: PathBuf, dir: &Path) -> Result<Self> {
        let relative_path = path.strip_prefix(dir)?;
        let file = fs::File::open(&path)?;
        let metadata = file.metadata()?;

        let title_and_info = info_from_path(&path)?;
        let pdf_document = pdf::file::FileOptions::cached().open(&path)?;
        let pages = pdf_document.num_pages();

        Ok(Self {
            title: title_and_info.title,
            relative_path: relative_path.to_str().context("invalid path")?.to_owned(),
            path,
            info: title_and_info.set_info,
            pages: pages as usize,
            size: metadata.len(),
            modified: metadata.modified()?,
            source: title_and_info.source,
        })
    }

    fn is_pdf(&self) -> bool {
        self.path.extension().is_some_and(|e| e == "pdf")
    }

    fn name(&self) -> String {
        format!("{} {}", self.number(), self.title)
    }

    fn source(&self) -> String {
        let source_url = self.source.as_ref().unwrap_or(&self.relative_path);
        static BRICKSET_LIBRARY_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^(?:https?://)?(images\.)?brickset\.com/library/").unwrap()
        });
        if BRICKSET_LIBRARY_RE.is_match(source_url) {
            return "Brickset Library".to_owned();
        }
        static BRICKSAFE_USER_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^(?:https?://)?bricksafe\.com/files/(.+?)/").unwrap());
        if let Some(info) = BRICKSAFE_USER_RE.captures(&self.relative_path) {
            return format!("Bricksafe / {}", info.get(1).unwrap().as_str());
        }
        static PEERON_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^(?:https?://)?(www\.)?peeron\.com/").unwrap());
        if PEERON_RE.is_match(source_url) {
            return "Peeron".to_owned();
        }
        static LEGO_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^(?:https?://)?(www\.)?lego\.com/").unwrap());
        if LEGO_RE.is_match(source_url) {
            return "Lego".to_owned();
        }
        "".to_owned()
    }

    fn number(&self) -> &str {
        self.info.as_ref().map_or("", |i| &i.number)
    }

    fn genres(&self) -> &[String] {
        match &self.info {
            Some(info) => &info.themes,
            None => <&[String]>::default(),
        }
    }

    fn year(&self) -> &str {
        self.info.as_ref().map_or("", |i| &i.year)
    }

    fn view_url(&self) -> String {
        format!("/view/{}", encode_path_segment(self.relative_path.as_str()),)
    }

    fn version(&self) -> String {
        format!(
            "{}",
            self.modified.duration_since(UNIX_EPOCH).unwrap().as_secs()
        )
    }
}

fn genre_search_url(genre: &str) -> String {
    IndexQuery::default()
        .with_genre_filter(Some(genre.to_string()))
        .to_url()
}

fn year_search_url(year: &str) -> String {
    IndexQuery::default()
        .with_year_filter(Some(year.to_string()))
        .to_url()
}

fn find_files(dir: &Path) -> Result<Vec<PathBuf>, io::Error> {
    fn collect_files(parent: &Path, results: &mut Vec<PathBuf>) -> Result<(), io::Error> {
        for entry in fs::read_dir(parent)? {
            let path = entry?.path();
            if path.is_dir() {
                collect_files(&path, results)?
            } else if path
                .extension()
                .is_some_and(|ext| ext == "cbz" || ext == "pdf")
            {
                results.push(path)
            }
        }
        Ok(())
    }

    let mut results: Vec<PathBuf> = vec![];
    collect_files(dir, &mut results)?;
    Ok(results)
}

fn format_bytes(value: u64) -> String {
    let value = byte_unit::Byte::from_bytes(value.into()).get_appropriate_unit(false);
    let digits = match value.get_value() {
        v if v < 1. => 2,
        v if v < 10. => 1,
        _ => 0,
    };
    value.format(digits)
}

#[derive(TemplateSimple)]
#[template(path = "index.stpl")]
struct IndexTemplate<'a> {
    sets: Vec<&'a SetAggregate>,
    unmatched_files: Vec<&'a File>,
    query: IndexQuery,
    all_years: &'a BTreeSet<String>,
    all_genres: &'a BTreeSet<String>,
    all_link: Option<String>,
}

#[derive(TemplateSimple)]
#[template(path = "set.stpl")]
struct SetTemplate<'a> {
    set: &'a SetInfo,
    files: Vec<&'a File>,
}

#[derive(TemplateSimple)]
#[template(path = "view.stpl")]
struct ViewTemplate<'a> {
    file: &'a File,
    image_url: String,
    next_url: Option<String>,
    previous_url: Option<String>,
}

#[derive(TemplateSimple)]
#[template(path = "view_pdf.stpl")]
struct PdfViewTemplate<'a> {
    file: &'a File,
    image_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();
    let dir = match args.dir {
        None => env::current_dir()?,
        Some(path) => path.into(),
    };
    let entries = find_files(&dir)?;
    println!("found {} files", entries.len());
    let files = entries
        .into_iter()
        .flat_map(|e| match File::from_path(e, &dir) {
            Ok(file) => Some(file),
            Err(err) => {
                eprintln!("Error while reading file: {}", err);
                None
            }
        })
        .collect::<Vec<_>>();

    let shared_state: SharedState = Arc::new(RwLock::new(AppState::from_files(files)));

    let app = Router::new()
        .route("/", get(show_index))
        .route("/sets/", get(show_index))
        .route("/sets/{id}", get(show_set))
        .route("/view/{*path}", get(show_file))
        .nest_service("/assets", ServeDir::new("assets"))
        .with_state(shared_state);

    let sock_addr = SocketAddr::from((IpAddr::from_str(args.listen.as_str())?, args.port));
    println!("listening on http://{}", sock_addr);
    let listener = TcpListener::bind(sock_addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

#[serde_as]
#[derive(Clone, Deserialize, Default)]
struct IndexQuery {
    #[serde_as(as = "NoneAsEmptyString")]
    #[serde(default)]
    genre: Option<String>,
    #[serde_as(as = "NoneAsEmptyString")]
    #[serde(default)]
    year: Option<String>,
    #[serde_as(as = "NoneAsEmptyString")]
    #[serde(default)]
    sort: Option<FileSort>,
    #[serde(default)]
    all: bool,
}

impl IndexQuery {
    fn with_sort(self, sort: Option<FileSort>) -> Self {
        Self { sort, ..self }
    }

    fn with_genre_filter(self, genre: Option<String>) -> Self {
        Self { genre, ..self }
    }

    fn with_year_filter(self, year: Option<String>) -> Self {
        Self { year, ..self }
    }

    fn with_all(self, all: bool) -> Self {
        Self { all, ..self }
    }

    fn to_url(&self) -> String {
        let base = "/?";
        let mut query = form_urlencoded::Serializer::for_suffix(String::from(base), base.len());
        self.genre
            .as_ref()
            .map(|g| query.append_pair("genre", g.as_str()));
        self.year
            .as_ref()
            .map(|y| query.append_pair("year", y.as_str()));
        self.sort
            .map(|s| query.append_pair("sort", s.to_query().as_str()));
        if self.all {
            query.append_pair("all", "true");
        }
        query.finish()
    }
}

// TODO: can render_sort_link be made generic?
// #[serde_as]
// #[derive(Clone, Deserialize, Default)]
// struct SetQuery {
//     #[serde_as(as = "NoneAsEmptyString")]
//     #[serde(default)]
//     sort: Option<FileSort>,
// }
//
// impl SetQuery {
//     fn with_sort(self, sort: Option<FileSort>) -> Self {
//         Self { sort, ..self }
//     }
// }

fn render_sort_link(query: &IndexQuery, field: FileField, title: &str) -> String {
    if query.sort.is_some_and(|s| s.field == field) {
        let (current, next) = match query.sort.unwrap().direction {
            Direction::Ascending => ("↑", Direction::Descending),
            Direction::Descending => ("↓", Direction::Ascending),
        };
        format!(
            "<a href=\"{}\">{}</a><span>{}</span>",
            query
                .clone()
                .with_sort(Some(FileSort {
                    direction: next,
                    field,
                }))
                .to_url(),
            title,
            current
        )
    } else {
        format!(
            "<a href=\"{}\">{}</a>",
            query
                .clone()
                .with_sort(Some(FileSort {
                    direction: Direction::Ascending,
                    field,
                }))
                .to_url(),
            title
        )
    }
}

fn render_file_count(count: usize) -> &'static str {
    match count {
        0 => "<span title=\"no files\">⚠</span>",
        1 => "",
        _ => "<span title=\"multiple files\">▤</span>",
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Direction {
    Ascending,
    Descending,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum FileField {
    Number,
    Name,
    Year,
    Genre,
    Pages,
    Size,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, DeserializeFromStr)]
struct FileSort {
    direction: Direction,
    field: FileField,
}

impl FileSort {
    fn to_query(&self) -> String {
        match self.direction {
            Direction::Ascending => self.field.to_string(),
            Direction::Descending => format!("-{}", self.field),
        }
    }
}

impl FromStr for FileSort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (direction, s) = match s.strip_prefix("-") {
            Some(s) => (Direction::Descending, s),
            None => (Direction::Ascending, s),
        };
        let field: FileField = s.parse()?;
        Ok(FileSort { direction, field })
    }
}

impl Display for FileField {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                FileField::Number => "number",
                FileField::Name => "name",
                FileField::Year => "year",
                FileField::Genre => "genre",
                FileField::Pages => "pages",
                FileField::Size => "size",
            }
        )
    }
}

impl FromStr for FileField {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "number" => Ok(FileField::Number),
            "name" => Ok(FileField::Name),
            "year" => Ok(FileField::Year),
            "genre" => Ok(FileField::Genre),
            "pages" => Ok(FileField::Pages),
            "size" => Ok(FileField::Size),
            _ => Err(format!("Invalid FileField '{}'", s)),
        }
    }
}

async fn show_index(
    State(state): State<SharedState>,
    Query(query): Query<IndexQuery>,
) -> Result<Html<String>, InternalError> {
    let all_view_supported = query.genre.is_some() || query.year.is_some();
    let query = if query.all && !all_view_supported {
        // redirect instead?
        query.with_all(false)
    } else {
        query
    };

    let state = state.read().await;
    let mut sets = state
        .sets
        .iter()
        .filter(|f| match &query.genre {
            Some(genre) => f.info.themes.contains(genre),
            _ => true,
        })
        .filter(|f| match &query.year {
            Some(year) => &f.info.year == year,
            _ => true,
        })
        .collect::<Vec<_>>();

    let missing = if all_view_supported {
        let found = sets.iter().map(|s| &s.info.number).collect::<HashSet<_>>();
        SETS.values()
            .filter(|info| match &query.genre {
                Some(genre) => theme_names(info.theme()).contains(genre),
                _ => true,
            })
            .filter(|info| match &query.year {
                Some(year) => &info.year == year,
                _ => true,
            })
            .filter(|info| !found.contains(&info.set_num))
            .map(|i| SetAggregate {
                info: i.into(),
                files: vec![],
            })
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    if query.all {
        sets.extend(&missing);
    };

    let all_link = if query.all {
        Some(format!(
            "<a href=\"{}\">Hide sets without files</a>",
            query.clone().with_all(false).to_url()
        ))
    } else if all_view_supported && !missing.is_empty() {
        Some(format!(
            "<a href=\"{}\">Show sets without files</a>",
            query.clone().with_all(true).to_url()
        ))
    } else {
        None
    };

    let unmatched_files = state
        .files
        .iter()
        .filter(|f| f.info.is_none())
        .filter(|_f| query.genre.is_none()) // bare files can't have genre
        .filter(|_f| query.year.is_none()) // bare files can't have year
        .collect::<Vec<_>>();

    let sort = query.sort.unwrap_or(FileSort {
        direction: Direction::Ascending,
        field: FileField::Number,
    });
    match sort.field {
        FileField::Number => sets.sort_by_key(|f| split_name(&f.info.number)),
        FileField::Name => sets.sort_by_key(|f| f.info.title.to_ascii_lowercase()),
        FileField::Year => sets.sort_by_key(|f| &f.info.year),
        FileField::Genre => sets.sort_by_key(|f| &f.info.themes),
        _ => {}
    }
    if sort.direction == Direction::Descending {
        sets.reverse()
    }

    let ctx = IndexTemplate {
        sets,
        unmatched_files,
        query,
        all_years: &state.all_years,
        all_genres: &state.all_genres,
        all_link,
    };
    Ok(Html(ctx.render_once()?))
}

async fn show_set(
    State(state): State<SharedState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Result<Response, InternalError> {
    let state = state.read().await;
    let set = state.get_set_by_number(&path);
    let empty_set = if set.is_none() {
        SETS.get(&path).map(SetInfo::from)
    } else {
        None
    };
    let set = match (set, &empty_set) {
        (Some(set), _) => set,
        (_, Some(set)) => set,
        _ => return Ok(StatusCode::NOT_FOUND.into_response()),
    };
    let files = state.get_files_for_set(&set);

    let ctx = SetTemplate { set, files };
    Ok(Html(ctx.render_once()?).into_response())
}

fn split_name(name: &str) -> (u32, &str) {
    if let Some(first_nonnumber) = name.find(|ch: char| !ch.is_ascii_digit()) {
        let (num, rest) = name.split_at(first_nonnumber);
        if let Ok(num) = num.parse() {
            return (num, rest);
        }
    }
    (u32::MAX, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_info_from_filename() {
        assert_eq!(info_from_filename("8891-1").title, "Idea Book 8891");
        assert_eq!(info_from_filename("8891").title, "Idea Book 8891");
        assert_eq!(info_from_filename("8891 (1)").title, "Idea Book 8891 (1)");
        assert_eq!(
            info_from_filename("6848%20Strategic%20Pursuer.pdf").title,
            "Strategic Pursuer"
        );
        assert_eq!(
            info_from_filename("6849%20Satellite%20Patroller.pdf").title,
            "Satellite Patroller"
        );
    }

    #[test]
    fn test_split_name() {
        assert_eq!(split_name("123 Hello"), (123, " Hello"));
        assert_eq!(split_name("Hello"), (u32::MAX, "Hello"));
        assert_eq!(split_name("Hello 123"), (u32::MAX, "Hello 123"));
    }
}

#[derive(Deserialize)]
struct ShowFileQuery {
    raw: Option<String>,
}

fn should_expose(filename: &str) -> bool {
    filename.ends_with(".jpg") || filename.ends_with(".gif")
}

#[debug_handler]
async fn show_file(
    State(state): State<SharedState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    Query(query): Query<ShowFileQuery>,
) -> Result<Response, InternalError> {
    let state = state.read().await;

    let file = state
        .files
        .iter()
        .find(|&f| path.starts_with(&f.relative_path));
    if file.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let file = file.unwrap();
    if file.is_pdf() {
        show_pdf(file, path, query).await
    } else {
        show_cbz(file, path, query)
    }
}

fn show_cbz(file: &File, path: String, query: ShowFileQuery) -> Result<Response, InternalError> {
    let mut zip = ZipArchive::new(fs::File::open(&file.path)?)?;

    let mut pages: Vec<&str> = zip.file_names().filter(|f| should_expose(f)).collect();
    pages.sort();
    if pages.is_empty() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    let subpath = path.strip_prefix(&file.relative_path);
    let page_index = if subpath.is_some_and(|s| s != "") {
        let subpath = subpath.unwrap();
        if !(subpath.starts_with("/") && should_expose(subpath)) {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }

        let subpath = subpath.strip_prefix("/").unwrap();
        let page_index = pages.iter().position(|p| p == &subpath);
        if !page_index.is_some() {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        if query.raw.is_some() {
            let mut page = zip.by_name(subpath)?;

            let mut data = vec![];
            page.read_to_end(&mut data)?;

            let content_type = if subpath.ends_with(".gif") {
                "image/gif"
            } else {
                "image/jpeg"
            };

            return Ok((
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, "public, max-age=31536000"),
                    (
                        header::LAST_MODIFIED,
                        &http_date_from_zip(page.last_modified())?,
                    ),
                ],
                data,
            )
                .into_response());
        }
        page_index.unwrap()
    } else {
        0
    };

    let previous = if page_index > 0 {
        pages.get(page_index - 1)
    } else {
        None
    };
    let current = pages[page_index];
    let next = pages.get(page_index + 1);

    let ctx = ViewTemplate {
        file,
        image_url: format!(
            "/view/{}/{}?raw&v={}",
            encode_path_segment(file.relative_path.as_str()),
            encode_path_segment(current),
            file.version(),
        ),
        next_url: next.map(|next| {
            format!(
                "/view/{}/{}",
                encode_path_segment(file.relative_path.as_str()),
                encode_path_segment(next)
            )
        }),
        previous_url: previous.map(|previous| {
            format!(
                "/view/{}/{}",
                encode_path_segment(file.relative_path.as_str()),
                encode_path_segment(previous)
            )
        }),
    };
    Ok(Html(ctx.render_once()?).into_response())
}

async fn show_pdf(
    file: &File,
    path: String,
    query: ShowFileQuery,
) -> Result<Response, InternalError> {
    // can't navigate to specific pages in the PDF
    if path != file.relative_path {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if query.raw.is_some() {
        let file_ = tokio::fs::File::open(&file.path).await?;
        let stream = ReaderStream::new(file_);

        return Ok((
            [
                (header::CONTENT_TYPE, "application/pdf"),
                (header::CACHE_CONTROL, "public, max-age=31536000"),
                (header::LAST_MODIFIED, &fmt_http_date(file.modified)),
            ],
            Body::from_stream(stream),
        )
            .into_response());
    };

    let ctx = PdfViewTemplate {
        file,
        image_url: format!(
            "/view/{}?raw&v={}",
            encode_path_segment(file.relative_path.as_str()),
            file.version(),
        ),
    };
    Ok(Html(ctx.render_once()?).into_response())
}

fn http_date_from_zip(date: Option<zip::DateTime>) -> Result<String> {
    let date = date.context("missing last-modified")?;
    let date = NaiveDateTime::try_from(date)?;
    Ok(fmt_http_date(date.and_utc().into()))
}

fn encode_path_segment<'a>(str: &'a str) -> PercentEncode<'a> {
    utf8_percent_encode(str, NON_ALPHANUMERIC)
}

struct InternalError(anyhow::Error);

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for InternalError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
