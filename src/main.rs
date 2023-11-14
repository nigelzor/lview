use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{extract::State, response::Html, routing::get, Router};
use clap::Parser;
use lazy_static::lazy_static;
use serde::de::Visitor;
use serde::{de, Deserialize, Deserializer, Serialize};
use std::fmt::Display;
use std::io::{BufReader, Read};
use std::marker::PhantomData;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::{env, fmt, fs, io};
use tera::{Context, Tera};
use zip::ZipArchive;

/// Serve cbz files from directory
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// Directory to serve
    #[arg(long)]
    dir: Option<String>,

    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    port: u16,
}

type SharedState = Arc<RwLock<AppState>>;

#[derive(Debug)]
struct AppState {
    files: Vec<File>,
}

#[derive(Debug, Serialize)]
struct File {
    name: String,
    relative_path: String,
    path: PathBuf,
    info: Option<ComicInfo>,
}

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
    #[serde(rename = "Genre", deserialize_with = "comma_separated")]
    genre: Vec<String>,
    #[serde(rename = "Web")]
    web: String,
}

fn comma_separated<'de, V, T, D>(deserializer: D) -> Result<V, D::Error>
where
    V: FromIterator<T>,
    T: FromStr,
    T::Err: Display,
    D: Deserializer<'de>,
{
    struct CommaSeparated<V, T>(PhantomData<V>, PhantomData<T>);

    impl<'de, V, T> Visitor<'de> for CommaSeparated<V, T>
    where
        V: FromIterator<T>,
        T: FromStr,
        T::Err: Display,
    {
        type Value = V;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string containing comma-separated elements")
        }

        fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let iter = s.split(",").map(FromStr::from_str);
            Result::from_iter(iter).map_err(de::Error::custom)
        }
    }

    let visitor = CommaSeparated(PhantomData, PhantomData);
    deserializer.deserialize_str(visitor)
}

impl File {
    fn from_path(path: PathBuf, dir: &Path) -> Result<Self, anyhow::Error> {
        let file = fs::File::open(path.as_path())?;
        let mut zip = ZipArchive::new(file)?;
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
        })
    }
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

lazy_static! {
    static ref TEMPLATES: Tera = Tera::new("templates/**/*.html").unwrap();
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

    let shared_state: SharedState = Arc::new(RwLock::new(AppState { files }));

    let app = Router::new()
        .route("/", get(show_index))
        .route("/view/*path", get(show_cbz))
        .with_state(Arc::clone(&shared_state));

    let sock_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port));
    println!("listening on http://{}", sock_addr);
    axum::Server::bind(&sock_addr)
        .serve(app.into_make_service())
        .await
        .unwrap()
}

#[derive(Deserialize)]
struct IndexQuery {
    genre: Option<String>,
    year: Option<String>,
}

async fn show_index(
    State(state): State<SharedState>,
    query: axum::extract::Query<IndexQuery>,
) -> Html<String> {
    let mut context = Context::new();
    let state = state.read().unwrap();
    let mut files = state
        .files
        .iter()
        .filter(|f| match &query.genre {
            Some(genre) => genre == "" || f.info.as_ref().is_some_and(|i| i.genre.contains(&genre)),
            _ => true,
        })
        .filter(|f| match &query.year {
            Some(year) => year == "" || f.info.as_ref().is_some_and(|i| &i.year == year),
            _ => true,
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|f| &f.name);

    context.insert("files", &files);
    Html(TEMPLATES.render("index.html", &context).unwrap())
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
    let mut context = Context::new();
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

    context.insert("file", file);
    context.insert("pages", &pages);
    context.insert("previous", &previous);
    context.insert("current", &current);
    context.insert("next", &next);
    Html(TEMPLATES.render("view.html", &context).unwrap()).into_response()
}
