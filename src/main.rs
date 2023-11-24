use axum::extract::Query;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{extract::State, response::Html, routing::get, Router};
use clap::Parser;
use percent_encoding::{utf8_percent_encode, PercentEncode, NON_ALPHANUMERIC};
use sailfish::TemplateOnce;
use serde::{Deserialize, Serialize};
use serde_with::formats::CommaSeparator;
use serde_with::{serde_as, DeserializeFromStr, NoneAsEmptyString, StringWithSeparator};
use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::io::{BufReader, Read};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::{env, fmt, fs, io};
use zip::ZipArchive;

/// Serve cbz files from directory
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// Directory to serve
    #[arg(long)]
    dir: Option<String>,

    // Address to listen on
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,

    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    port: u16,
}

type SharedState = Arc<RwLock<AppState>>;

#[derive(Debug)]
struct AppState {
    files: Vec<File>,
    all_years: BTreeSet<String>,
    all_genres: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
struct File {
    name: String,
    relative_path: String,
    path: PathBuf,
    info: Option<ComicInfo>,
    pages: usize,
    size: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
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

impl File {
    fn from_path(path: PathBuf, dir: &Path) -> Result<Self, anyhow::Error> {
        let file = fs::File::open(path.as_path())?;
        let metadata = file.metadata()?;

        let mut zip = ZipArchive::new(file)?;
        let pages = zip.file_names().filter(|f| should_expose(f)).count();
        let (name, info) = match zip.by_name("ComicInfo.xml") {
            Ok(info_xml) => {
                let info: ComicInfo = quick_xml::de::from_reader(BufReader::new(info_xml))?;
                // println!("{:?}", info);
                (format!("{} {}", info.number, info.title), Some(info))
            }
            _ => (path.file_stem().unwrap().to_str().unwrap().into(), None),
        };

        Ok(Self {
            name,
            relative_path: path.strip_prefix(dir).unwrap().to_str().unwrap().into(),
            path,
            info,
            pages,
            size: metadata.len(),
        })
    }

    fn genres(&self) -> &[String] {
        if self.info.is_none() {
            return <&[String]>::default();
        }
        &self.info.as_ref().unwrap().genre
    }

    fn year(&self) -> &str {
        self.info.as_ref().map_or("", |i| &i.year)
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
    // TODO: make recursive
    fs::read_dir(dir)?
        .map(|res| res.map(|e| e.path()))
        .filter(|p| {
            p.as_ref()
                .is_ok_and(|b| b.extension().is_some_and(|ext| ext == "cbz"))
        })
        .collect::<Result<Vec<_>, io::Error>>()
}

fn format_bytes(value: u64) -> String {
    byte_unit::Byte::from_bytes(value.into())
        .get_appropriate_unit(false)
        .to_string()
}

#[derive(TemplateOnce)]
#[template(path = "index.stpl")]
struct IndexTemplate<'a> {
    files: Vec<&'a File>,
    query: IndexQuery,
    all_years: &'a BTreeSet<String>,
    all_genres: &'a BTreeSet<String>,
}

#[derive(TemplateOnce)]
#[template(path = "view.stpl")]
struct ViewTemplate<'a> {
    file: &'a File,
    image_url: String,
    next_url: Option<String>,
    previous_url: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = CliArgs::parse();
    let dir = match args.dir {
        None => env::current_dir().unwrap(),
        Some(path) => path.into(),
    };
    let entries = find_files(dir.as_path()).unwrap();
    let files = entries
        .into_iter()
        .map(|e| File::from_path(e, dir.as_path()))
        .collect::<Result<Vec<_>, anyhow::Error>>()
        .unwrap();

    // TODO: this over-allocates strings
    let all_years = files
        .iter()
        .map(|f| f.year().to_string())
        .collect::<BTreeSet<_>>();
    let all_genres = files
        .iter()
        .flat_map(|f| f.genres())
        .cloned()
        .collect::<BTreeSet<_>>();

    let shared_state: SharedState = Arc::new(RwLock::new(AppState {
        files,
        all_years,
        all_genres,
    }));

    let app = Router::new()
        .route("/", get(show_index))
        .route("/view/*path", get(show_cbz))
        .with_state(Arc::clone(&shared_state));

    let sock_addr = SocketAddr::from((IpAddr::from_str(args.listen.as_str()).unwrap(), args.port));
    println!("listening on http://{}", sock_addr);
    axum::Server::bind(&sock_addr)
        .serve(app.into_make_service())
        .await
        .unwrap()
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
        query.finish()
    }
}

fn render_sort_link(query: &IndexQuery, field: FileField, title: &str) -> String {
    if query.sort.is_some_and(|s| s.field == field) {
        let (current, next) = match query.sort.unwrap().direction {
            Direction::Ascending => ("↑", Direction::Descending),
            Direction::Descending => ("↓", Direction::Ascending),
        };
        format!(
            "<a href=\"{}\">{} <span>{}</span></a>",
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

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Direction {
    Ascending,
    Descending,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum FileField {
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
        let field = s.parse::<FileField>()?;
        return Ok(FileSort { direction, field });
    }
}

impl Display for FileField {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
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
) -> Html<String> {
    let state = state.read().unwrap();
    let mut files = state
        .files
        .iter()
        .filter(|f| match &query.genre {
            Some(genre) => f.info.as_ref().is_some_and(|i| i.genre.contains(&genre)),
            _ => true,
        })
        .filter(|f| match &query.year {
            Some(year) => f.info.as_ref().is_some_and(|i| &i.year == year),
            _ => true,
        })
        .collect::<Vec<_>>();

    let sort = query.sort.unwrap_or(FileSort {
        direction: Direction::Ascending,
        field: FileField::Name,
    });
    match sort.field {
        FileField::Name => files.sort_by_key(|f| &f.name),
        FileField::Year => files.sort_by_key(|f| f.year()),
        FileField::Genre => files.sort_by_key(|f| f.genres()),
        FileField::Pages => files.sort_by_key(|f| f.pages),
        FileField::Size => files.sort_by_key(|f| f.size),
    }
    if sort.direction == Direction::Descending {
        files.reverse()
    }

    let ctx = IndexTemplate {
        files,
        query,
        all_years: &state.all_years,
        all_genres: &state.all_genres,
    };
    Html(ctx.render_once().unwrap())
}

#[derive(Deserialize)]
struct CbzQuery {
    raw: Option<String>,
}

fn should_expose(filename: &str) -> bool {
    return filename.ends_with(".jpg");
}

async fn show_cbz(
    State(state): State<SharedState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    query: axum::extract::Query<CbzQuery>,
) -> Response {
    let state = state.read().unwrap();

    let file = state
        .files
        .iter()
        .find(|&f| path.starts_with(&f.relative_path));
    if file.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = file.unwrap();

    let mut zip = ZipArchive::new(fs::File::open(file.path.as_path()).unwrap()).unwrap();

    let mut pages: Vec<&str> = zip.file_names().filter(|f| should_expose(f)).collect();
    pages.sort();

    let subpath = path.strip_prefix(&file.relative_path);
    let page_index = if subpath.is_some_and(|s| s != "") {
        let subpath = subpath.unwrap();
        if !(subpath.starts_with("/") && should_expose(subpath)) {
            return StatusCode::NOT_FOUND.into_response();
        }

        let subpath = subpath.strip_prefix("/").unwrap();
        let page_index = pages.iter().position(|p| p == &subpath);
        if !page_index.is_some() {
            return StatusCode::NOT_FOUND.into_response();
        }
        if query.raw.is_some() {
            let mut page = zip.by_name(subpath).unwrap();

            let mut data = vec![];
            let _length = page.read_to_end(&mut data).unwrap();

            // TODO: (header::DATE, page.last_modified())
            return ([(header::CONTENT_TYPE, "image/jpeg")], data).into_response();
        }
        page_index.unwrap()
    } else {
        0
    };

    let previous = if page_index == 0 {
        None
    } else {
        pages.get(page_index - 1)
    };
    let current = pages[page_index];
    let next = pages.get(page_index + 1);

    let ctx = ViewTemplate {
        file,
        image_url: format!(
            "/view/{}/{}?raw",
            encode_path_segment(file.relative_path.as_str()),
            encode_path_segment(current)
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
    Html(ctx.render_once().unwrap()).into_response()
}

fn encode_path_segment(str: &str) -> PercentEncode {
    utf8_percent_encode(str, NON_ALPHANUMERIC)
}
