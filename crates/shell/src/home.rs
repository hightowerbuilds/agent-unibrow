//! The home page at `unibrow://home/` (homepage roadmap H1): what opens on
//! launch and in new tabs. Its files are built into the app; the
//! `unibrow://` plumbing and its rules are in `rail_browser::app_pages`.

use rail_browser::app_pages::{AppPages, Page};

const INDEX: &str = include_str!("../home/index.html");
const CSS: &str = include_str!("../home/home.css");
const ICON: &str = include_str!("../home/favicon.svg");

pub struct Home;

impl AppPages for Home {
    fn get(&self, host: &str, path: &str) -> Page {
        match (host, path) {
            ("home", "/" | "/index.html") => Page::ok("text/html", INDEX),
            ("home", "/home.css") => Page::ok("text/css", CSS),
            ("home", "/favicon.svg") => Page::ok("image/svg+xml", ICON),
            _ => Page::not_found(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_home_files() {
        assert_eq!(Home.get("home", "/").mime, "text/html");
        assert!(String::from_utf8(Home.get("home", "/").body).unwrap().contains("<title>Home</title>"));
        assert_eq!(Home.get("home", "/home.css").status, 200);
        assert_eq!(Home.get("home", "/favicon.svg").mime, "image/svg+xml");
        assert_eq!(Home.get("home", "/secret").status, 404);
        assert_eq!(Home.get("settings", "/").status, 404);
    }

    #[test]
    fn page_loads_nothing_from_the_web() {
        // The only outside address is the search form's target.
        let html = INDEX.replace("action=\"https://duckduckgo.com/\"", "");
        assert!(!html.contains("http://") && !html.contains("https://"), "{html}");
        assert!(!html.contains("<script"));
    }
}
