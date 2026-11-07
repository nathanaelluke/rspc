use super::*;

fn label(value: &str) -> String {
    value.replace(['\n', '\r', '\0'], " ").replace('/', "∕")
}
fn encoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

impl Catalog {
    async fn people(self: &Arc<Self>) -> Result<BTreeMap<String, String>> {
        let mut cache = self.people_cache.lock().await;
        if let Some((at, people)) = cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
        {
            let _ = at;
            return Ok(people.clone());
        }
        let mut people = BTreeMap::new();
        if self.demo {
            people.insert("Example Curator".into(), "demo-user".into());
        } else {
            let mut names = BTreeMap::<String, String>::new();
            // Followed people have display names. Owners of saved playlists
            // are also discoverable when they are not followed themselves.
            if let Ok(following) = self.get("me/following/users", &[]).await {
                for person in following["items"].as_array().into_iter().flatten() {
                    if let Some(uri) = person["uri"]
                        .as_str()
                        .and_then(|uri| uri.strip_prefix("spotify:user:"))
                    {
                        names.insert(
                            uri.into(),
                            person["name"]
                                .as_str()
                                .or(person["display_name"].as_str())
                                .unwrap_or(uri)
                                .into(),
                        );
                    }
                }
            }
            for playlist in self.pages("me/playlists").await? {
                let Some(id) = playlist["owner"]["id"].as_str().filter(|id| !id.is_empty()) else {
                    continue;
                };
                let display = playlist["owner"]["display_name"]
                    .as_str()
                    .filter(|name| *name != id);
                names.entry(id.into()).or_insert_with(|| {
                    display.map(str::to_owned).unwrap_or_else(|| {
                        format!(
                            "Curator of {}",
                            playlist["name"].as_str().unwrap_or("a saved playlist")
                        )
                    })
                });
            }
            for (id, name) in names {
                insert_named(&mut people, label(&name), id);
            }
        }
        *cache = Some((std::time::Instant::now(), people.clone()));
        Ok(people)
    }

    pub async fn browse_people(
        self: &Arc<Self>,
        path: &str,
    ) -> Result<(Vec<String>, Vec<TrackInfo>)> {
        if path == "People" {
            return Ok((
                self.people()
                    .await?
                    .into_iter()
                    .map(|(name, id)| format!("People/profile/{}/{name}", encoded(&id)))
                    .collect(),
                vec![],
            ));
        }
        let parts: Vec<_> = path.splitn(4, '/').collect();
        match parts.as_slice() {
            ["People", "profile", id, ..] => {
                if self.demo {
                    return Ok((vec!["People/playlist/demo/Demo Mix".into()], vec![]));
                }
                let profile = self.get(&format!("users/{id}/profile"), &[]).await?;
                // Profiles with no public playlists omit this field entirely.
                let empty = Vec::new();
                let items = if profile["public_playlists"].is_null() {
                    &empty
                } else {
                    profile["public_playlists"]
                        .as_array()
                        .or_else(|| profile["public_playlists"]["items"].as_array())
                        .context("Spotify returned an invalid public playlist list")?
                };
                let rules = self.playlist_settings.lock().unwrap().rules.clone();
                let mut directories = Vec::new();
                let mut names = HashMap::<String, usize>::new();
                for item in items {
                    let Some(uri) = item["uri"]
                        .as_str()
                        .filter(|uri| uri.starts_with("spotify:playlist:"))
                    else {
                        continue;
                    };
                    let name = item["name"].as_str().unwrap_or("Untitled playlist");
                    if rules.playlist(uri, name) {
                        continue;
                    }
                    let mut name = label(name);
                    let count = names.entry(name.clone()).or_default();
                    *count += 1;
                    if *count > 1 {
                        name.push_str(&format!(" ({count})"));
                    }
                    directories.push(format!(
                        "People/playlist/{}/{name}",
                        uri.rsplit(':').next().unwrap_or_default()
                    ));
                }
                Ok((directories, vec![]))
            }
            ["People", "playlist", id, ..] => {
                if self.demo {
                    return Ok((vec![], self.liked().await?));
                }
                if self.demo {
                    return Ok((vec![], self.liked().await?));
                }
                ensure!(
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
                    "invalid playlist ID"
                );
                let items = self.pages(&format!("playlists/{id}/tracks")).await?;
                let tracks = items
                    .iter()
                    .filter_map(|item| {
                        track_from_json(&item["item"]).or_else(|| track_from_json(&item["track"]))
                    })
                    .collect();
                Ok((vec![], self.remember(tracks).await))
            }
            _ => bail!("unknown person or playlist"),
        }
    }
}
