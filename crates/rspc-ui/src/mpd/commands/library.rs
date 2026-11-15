use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct LibraryItem {
    pub uri: String,
    pub name: String,
    pub saved: bool,
}
