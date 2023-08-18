use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use anyhow::{Context, Result};
use client::proto;
use globset::{Glob, GlobMatcher};
use itertools::Itertools;
use language::{char_kind, Rope};
use regex::{Regex, RegexBuilder};
use smol::future::yield_now;
use std::{
    io::{BufRead, BufReader, Read},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct SearchInputs {
    query: Arc<str>,
    files_to_include: Vec<PathMatcher>,
    files_to_exclude: Vec<PathMatcher>,
}

impl SearchInputs {
    pub fn as_str(&self) -> &str {
        self.query.as_ref()
    }
    pub fn files_to_include(&self) -> &[PathMatcher] {
        &self.files_to_include
    }
    pub fn files_to_exclude(&self) -> &[PathMatcher] {
        &self.files_to_exclude
    }
}
#[derive(Clone, Debug)]
pub enum SearchQuery {
    Text {
        search: Arc<AhoCorasick<usize>>,
        whole_word: bool,
        case_sensitive: bool,
        inner: SearchInputs,
    },
    Regex {
        regex: Regex,

        multiline: bool,
        whole_word: bool,
        case_sensitive: bool,
        inner: SearchInputs,
    },
}

#[derive(Clone, Debug)]
pub struct PathMatcher {
    maybe_path: PathBuf,
    glob: GlobMatcher,
}

impl std::fmt::Display for PathMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.maybe_path.to_string_lossy().fmt(f)
    }
}

impl PathMatcher {
    pub fn new(maybe_glob: &str) -> Result<Self, globset::Error> {
        Ok(PathMatcher {
            glob: Glob::new(&maybe_glob)?.compile_matcher(),
            maybe_path: PathBuf::from(maybe_glob),
        })
    }

    pub fn is_match<P: AsRef<Path>>(&self, other: P) -> bool {
        other.as_ref().starts_with(&self.maybe_path) || self.glob.is_match(other)
    }
}

impl SearchQuery {
    pub fn text(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        files_to_include: Vec<PathMatcher>,
        files_to_exclude: Vec<PathMatcher>,
    ) -> Self {
        let query = query.to_string();
        let search = AhoCorasickBuilder::new()
            .auto_configure(&[&query])
            .ascii_case_insensitive(!case_sensitive)
            .build(&[&query]);
        let inner = SearchInputs {
            query: query.into(),
            files_to_exclude,
            files_to_include,
        };
        Self::Text {
            search: Arc::new(search),
            whole_word,
            case_sensitive,
            inner,
        }
    }

    pub fn regex(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        files_to_include: Vec<PathMatcher>,
        files_to_exclude: Vec<PathMatcher>,
    ) -> Result<Self> {
        let mut query = query.to_string();
        let initial_query = Arc::from(query.as_str());
        if whole_word {
            let mut word_query = String::new();
            word_query.push_str("\\b");
            word_query.push_str(&query);
            word_query.push_str("\\b");
            query = word_query
        }

        let multiline = query.contains('\n') || query.contains("\\n");
        let regex = RegexBuilder::new(&query)
            .case_insensitive(!case_sensitive)
            .multi_line(multiline)
            .build()?;
        let inner = SearchInputs {
            query: initial_query,
            files_to_exclude,
            files_to_include,
        };
        Ok(Self::Regex {
            regex,
            multiline,
            whole_word,
            case_sensitive,
            inner,
        })
    }

    pub fn from_proto(message: proto::SearchProject) -> Result<Self> {
        if message.regex {
            Self::regex(
                message.query,
                message.whole_word,
                message.case_sensitive,
                deserialize_path_matches(&message.files_to_include)?,
                deserialize_path_matches(&message.files_to_exclude)?,
            )
        } else {
            Ok(Self::text(
                message.query,
                message.whole_word,
                message.case_sensitive,
                deserialize_path_matches(&message.files_to_include)?,
                deserialize_path_matches(&message.files_to_exclude)?,
            ))
        }
    }

    pub fn to_proto(&self, project_id: u64) -> proto::SearchProject {
        proto::SearchProject {
            project_id,
            query: self.as_str().to_string(),
            regex: self.is_regex(),
            whole_word: self.whole_word(),
            case_sensitive: self.case_sensitive(),
            files_to_include: self
                .files_to_include()
                .iter()
                .map(|matcher| matcher.to_string())
                .join(","),
            files_to_exclude: self
                .files_to_exclude()
                .iter()
                .map(|matcher| matcher.to_string())
                .join(","),
        }
    }

    pub fn detect<T: Read>(&self, stream: T) -> Result<bool> {
        if self.as_str().is_empty() {
            return Ok(false);
        }

        match self {
            Self::Text { search, .. } => {
                let mat = search.stream_find_iter(stream).next();
                match mat {
                    Some(Ok(_)) => Ok(true),
                    Some(Err(err)) => Err(err.into()),
                    None => Ok(false),
                }
            }
            Self::Regex {
                regex, multiline, ..
            } => {
                let mut reader = BufReader::new(stream);
                if *multiline {
                    let mut text = String::new();
                    if let Err(err) = reader.read_to_string(&mut text) {
                        Err(err.into())
                    } else {
                        Ok(regex.find(&text).is_some())
                    }
                } else {
                    for line in reader.lines() {
                        let line = line?;
                        if regex.find(&line).is_some() {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
            }
        }
    }

    pub async fn search(&self, rope: &Rope) -> Vec<Range<usize>> {
        const YIELD_INTERVAL: usize = 20000;

        if self.as_str().is_empty() {
            return Default::default();
        }

        let mut matches = Vec::new();
        match self {
            Self::Text {
                search, whole_word, ..
            } => {
                for (ix, mat) in search
                    .stream_find_iter(rope.bytes_in_range(0..rope.len()))
                    .enumerate()
                {
                    if (ix + 1) % YIELD_INTERVAL == 0 {
                        yield_now().await;
                    }

                    let mat = mat.unwrap();
                    if *whole_word {
                        let prev_kind = rope.reversed_chars_at(mat.start()).next().map(char_kind);
                        let start_kind = char_kind(rope.chars_at(mat.start()).next().unwrap());
                        let end_kind = char_kind(rope.reversed_chars_at(mat.end()).next().unwrap());
                        let next_kind = rope.chars_at(mat.end()).next().map(char_kind);
                        if Some(start_kind) == prev_kind || Some(end_kind) == next_kind {
                            continue;
                        }
                    }
                    matches.push(mat.start()..mat.end())
                }
            }
            Self::Regex {
                regex, multiline, ..
            } => {
                if *multiline {
                    let text = rope.to_string();
                    for (ix, mat) in regex.find_iter(&text).enumerate() {
                        if (ix + 1) % YIELD_INTERVAL == 0 {
                            yield_now().await;
                        }

                        matches.push(mat.start()..mat.end());
                    }
                } else {
                    let mut line = String::new();
                    let mut line_offset = 0;
                    for (chunk_ix, chunk) in rope.chunks().chain(["\n"]).enumerate() {
                        if (chunk_ix + 1) % YIELD_INTERVAL == 0 {
                            yield_now().await;
                        }

                        for (newline_ix, text) in chunk.split('\n').enumerate() {
                            if newline_ix > 0 {
                                for mat in regex.find_iter(&line) {
                                    let start = line_offset + mat.start();
                                    let end = line_offset + mat.end();
                                    matches.push(start..end);
                                }

                                line_offset += line.len() + 1;
                                line.clear();
                            }
                            line.push_str(text);
                        }
                    }
                }
            }
        }
        matches
    }

    pub fn as_str(&self) -> &str {
        self.as_inner().as_str()
    }

    pub fn whole_word(&self) -> bool {
        match self {
            Self::Text { whole_word, .. } => *whole_word,
            Self::Regex { whole_word, .. } => *whole_word,
        }
    }

    pub fn case_sensitive(&self) -> bool {
        match self {
            Self::Text { case_sensitive, .. } => *case_sensitive,
            Self::Regex { case_sensitive, .. } => *case_sensitive,
        }
    }

    pub fn is_regex(&self) -> bool {
        matches!(self, Self::Regex { .. })
    }

    pub fn files_to_include(&self) -> &[PathMatcher] {
        self.as_inner().files_to_include()
    }

    pub fn files_to_exclude(&self) -> &[PathMatcher] {
        self.as_inner().files_to_exclude()
    }

    pub fn file_matches(&self, file_path: Option<&Path>) -> bool {
        match file_path {
            Some(file_path) => {
                !self
                    .files_to_exclude()
                    .iter()
                    .any(|exclude_glob| exclude_glob.is_match(file_path))
                    && (self.files_to_include().is_empty()
                        || self
                            .files_to_include()
                            .iter()
                            .any(|include_glob| include_glob.is_match(file_path)))
            }
            None => self.files_to_include().is_empty(),
        }
    }
    pub fn as_inner(&self) -> &SearchInputs {
        match self {
            Self::Regex { inner, .. } | Self::Text { inner, .. } => inner,
        }
    }
}

fn deserialize_path_matches(glob_set: &str) -> anyhow::Result<Vec<PathMatcher>> {
    glob_set
        .split(',')
        .map(str::trim)
        .filter(|glob_str| !glob_str.is_empty())
        .map(|glob_str| {
            PathMatcher::new(glob_str)
                .with_context(|| format!("deserializing path match glob {glob_str}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matcher_creation_for_valid_paths() {
        for valid_path in [
            "file",
            "Cargo.toml",
            ".DS_Store",
            "~/dir/another_dir/",
            "./dir/file",
            "dir/[a-z].txt",
            "../dir/filé",
        ] {
            let path_matcher = PathMatcher::new(valid_path).unwrap_or_else(|e| {
                panic!("Valid path {valid_path} should be accepted, but got: {e}")
            });
            assert!(
                path_matcher.is_match(valid_path),
                "Path matcher for valid path {valid_path} should match itself"
            )
        }
    }

    #[test]
    fn path_matcher_creation_for_globs() {
        for invalid_glob in ["dir/[].txt", "dir/[a-z.txt", "dir/{file"] {
            match PathMatcher::new(invalid_glob) {
                Ok(_) => panic!("Invalid glob {invalid_glob} should not be accepted"),
                Err(_expected) => {}
            }
        }

        for valid_glob in [
            "dir/?ile",
            "dir/*.txt",
            "dir/**/file",
            "dir/[a-z].txt",
            "{dir,file}",
        ] {
            match PathMatcher::new(valid_glob) {
                Ok(_expected) => {}
                Err(e) => panic!("Valid glob {valid_glob} should be accepted, but got: {e}"),
            }
        }
    }
}
