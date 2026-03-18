use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::sync::LazyLock;

pub struct Theme {
    id: u32,
    pub name: String,
    parent: Option<u32>,
}
impl Theme {
    pub fn parent(&self) -> Option<&Theme> {
        if let Some(parent) = self.parent {
            THEMES.get(&parent)
        } else {
            None
        }
    }

    pub fn with_parents(&self) -> Vec<&Theme> {
        let mut res = vec![];
        let mut current = Some(self);
        while current.is_some() {
            res.insert(0, current.unwrap());
            current = current.unwrap().parent();
        }
        res
    }
}

fn load_themes() -> Result<BTreeMap<u32, Theme>> {
    let file = File::open("themes.csv.gz")?;
    let reader = MultiGzDecoder::new(file);
    let mut csv = csv::Reader::from_reader(reader);

    let mut themes = BTreeMap::new();
    for row in csv.records() {
        let record = row?;
        let id: u32 = record[0].parse()?;
        let name = record[1].to_owned();
        let parent: Option<u32> = record[2].parse().ok();
        themes.insert(id, Theme { id, name, parent });
    }
    Ok(themes)
}

static THEMES: LazyLock<BTreeMap<u32, Theme>> =
    LazyLock::new(|| load_themes().expect("failed to load themes"));

pub struct SetInfo {
    pub set_num: String,
    pub name: String,
    pub year: String,
    theme_id: u32,
    pub num_parts: u32,
    pub img_url: String,
}
impl SetInfo {
    pub fn theme(&self) -> &Theme {
        THEMES.get(&self.theme_id).expect("missing theme")
    }
}

fn load_sets() -> Result<HashMap<String, SetInfo>> {
    let file = File::open("sets.csv.gz")?;
    let reader = MultiGzDecoder::new(file);
    let mut csv = csv::Reader::from_reader(reader);
    let mut sets = HashMap::new();
    for row in csv.records() {
        let record = row?;
        let set_num = record[0].to_owned();
        sets.insert(
            set_num.clone(),
            SetInfo {
                set_num,
                name: record[1].to_owned(),
                year: record[2].to_owned(),
                theme_id: record[3].parse()?,
                num_parts: record[4].parse()?,
                img_url: record[5].to_owned(),
            },
        );
    }
    Ok(sets)
}

pub static SETS: LazyLock<HashMap<String, SetInfo>> =
    LazyLock::new(|| load_sets().expect("failed to load sets"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_themes() {
        assert_eq!(THEMES.len(), 489);
    }

    #[test]
    fn test_load_sets() {
        assert_eq!(SETS.len(), 26186);
    }
}
