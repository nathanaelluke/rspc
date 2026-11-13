//! Parser for the tag predicates emitted by the library browsers and search pane.
use crate::model::TrackInfo;
use anyhow::{Result, bail, ensure};
#[derive(Debug, Clone)]
pub struct Predicate {
    pub tag: String,
    pub op: String,
    pub value: String,
    regex: Option<(regex::Regex, regex::Regex)>,
}
pub fn parse(input: &str) -> Result<Vec<Predicate>> {
    let mut c = input.chars().peekable();
    let mut result = Vec::new();
    while let Some(ch) = c.peek().copied() {
        if ch.is_whitespace() || ch == '(' || ch == ')' {
            c.next();
            continue;
        }
        let mut tag = String::new();
        while c
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        {
            tag.push(c.next().unwrap());
        }
        ensure!(!tag.is_empty(), "invalid filter token");
        if tag == "AND" {
            continue;
        }
        ensure!(
            tag != "OR" && tag != "NOT",
            "OR/NOT query groups are not supported by the Spotify adapter"
        );
        while c.peek().is_some_and(|c| c.is_whitespace()) {
            c.next();
        }
        let mut op = String::new();
        while c
            .peek()
            .is_some_and(|c| !c.is_whitespace() && *c != '\'' && *c != '"')
        {
            op.push(c.next().unwrap());
        }
        while c.peek().is_some_and(|c| c.is_whitespace()) {
            c.next();
        }
        let quote = c
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing filter value"))?;
        ensure!(
            quote == '\'' || quote == '"',
            "filter values must be quoted"
        );
        let mut value = String::new();
        let mut closed = false;
        while let Some(ch) = c.next() {
            if ch == '\\' {
                value.push(
                    c.next()
                        .ok_or_else(|| anyhow::anyhow!("truncated escape"))?,
                );
            } else if ch == quote {
                closed = true;
                break;
            } else {
                value.push(ch);
            }
        }
        ensure!(closed, "unterminated filter value");
        ensure!(
            ["==", "!=", "contains", "starts_with", "=~", "!~"].contains(&op.as_str()),
            "unsupported filter operation: {op}"
        );
        let regex = if op == "=~" || op == "!~" {
            Some((
                regex::Regex::new(&value)?,
                regex::RegexBuilder::new(&value)
                    .case_insensitive(true)
                    .build()?,
            ))
        } else {
            None
        };
        result.push(Predicate {
            tag: tag.to_lowercase(),
            op,
            value,
            regex,
        });
    }
    Ok(result)
}
fn visit_values(track: &TrackInfo, tag: &str, mut visit: impl FnMut(&str)) -> Result<()> {
    match tag {
        "artist" => track.artists.iter().for_each(|value| visit(value)),
        "albumartist" => track.album_artists().iter().for_each(|value| visit(value)),
        "album" => visit(&track.album),
        "genre" => track.genres.iter().for_each(|value| visit(value)),
        "title" => visit(&track.name),
        "file" | "filename" => visit(track.uri.as_deref().unwrap_or_default()),
        "date" | "originaldate" => visit(track.release_date.as_deref().unwrap_or_default()),
        "track" => visit(&track.track_number.to_string()),
        "any" => {
            visit(&track.name);
            visit(&track.album);
            visit(track.uri.as_deref().unwrap_or_default());
            track.artists.iter().for_each(|value| visit(value));
            track.genres.iter().for_each(|value| visit(value));
        }
        _ => bail!("Spotify does not expose the requested tag: {tag}"),
    }
    Ok(())
}

pub fn values(track: &TrackInfo, tag: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    visit_values(track, &tag.to_ascii_lowercase(), |value| {
        values.push(value.to_owned())
    })?;
    Ok(values)
}

pub fn matches(track: &TrackInfo, filters: &[Predicate], insensitive: bool) -> Result<bool> {
    use std::borrow::Cow;
    for p in filters {
        let fold = insensitive && !matches!(p.op.as_str(), "=~" | "!~");
        let value = if fold {
            Cow::Owned(p.value.to_lowercase())
        } else {
            Cow::Borrowed(p.value.as_str())
        };
        let mut found = false;
        visit_values(track, &p.tag, |candidate| {
            if found {
                return;
            }
            let candidate = if fold {
                Cow::Owned(candidate.to_lowercase())
            } else {
                Cow::Borrowed(candidate)
            };
            found = match p.op.as_str() {
                "==" | "!=" => candidate == value,
                "contains" => candidate.contains(value.as_ref()),
                "starts_with" => candidate.starts_with(value.as_ref()),
                "=~" | "!~" => {
                    let (sensitive, folded) = p.regex.as_ref().expect("parsed regex predicate");
                    let re = if insensitive { folded } else { sensitive };
                    re.is_match(&candidate)
                }
                _ => false,
            };
        })?;
        if found == matches!(p.op.as_str(), "!=" | "!~") {
            return Ok(false);
        }
    }
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_rmpc_escaped_tag_filters() {
        let f = parse("((Artist == 'John\\'s Band') AND (Album contains '第一'))").unwrap();
        assert_eq!(f[0].value, "John's Band");
        assert_eq!(f[1].value, "第一");
        assert!(parse("(Title contains 'broken)").is_err());
        assert!(parse("(Genre == 'x') OR (Title == 'y')").is_err());
    }
    #[test]
    fn search_is_insensitive_and_multiple_filters_are_anded() {
        let track = super::super::catalog::demo_tracks().remove(0);
        assert!(matches(&track, &parse("(Title contains 'morning')").unwrap(), true).unwrap());
        assert!(!matches(&track, &parse("(Title contains 'MORNING')").unwrap(), false).unwrap());
        assert!(
            !matches(
                &track,
                &parse("(Title contains 'morning') AND (Album == 'wrong')").unwrap(),
                true
            )
            .unwrap()
        );
        assert!(matches(&track, &parse(r"(Title =~ '\\p{Lu}orning')").unwrap(), true).unwrap());
    }
}
