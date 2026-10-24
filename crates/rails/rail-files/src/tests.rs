use super::*;
use crate::paths::tests::sandbox;
use router::{Approver, ToolImage};
use std::sync::Mutex;

/// Answers every card the same way and keeps what it was shown.
struct Cards {
    answer: bool,
    seen: Mutex<Vec<String>>,
}

impl Approver for Cards {
    fn approve(&self, _: &'static str, description: String, _: String, _: Vec<String>, _: Option<ToolImage>) -> BoxFuture<'static, bool> {
        self.seen.lock().unwrap().push(description);
        let a = self.answer;
        Box::pin(async move { a })
    }
}

fn ctx(answer: bool) -> (ToolContext, Arc<Cards>) {
    let cards = Arc::new(Cards { answer, seen: Mutex::new(Vec::new()) });
    (ToolContext { approver: cards.clone() }, cards)
}

fn tool(cfg: &Config, name: &str) -> Arc<dyn Tool> {
    FilesRail::new(cfg.clone()).tools().into_iter().find(|t| t.name() == name).unwrap()
}

async fn run(cfg: &Config, name: &str, input: Value, answer: bool) -> (ToolResult, Vec<String>) {
    let (ctx, cards) = ctx(answer);
    let r = tool(cfg, name).call_with(input, ctx).await;
    let seen = cards.seen.lock().unwrap().clone();
    (r, seen)
}

#[test]
fn replace_text_rules() {
    assert_eq!(replace_text("a b a", "b", "c", false).unwrap(), ("a c a".into(), 1));
    assert!(replace_text("a b a", "a", "c", false).unwrap_err().contains("2 times"));
    assert_eq!(replace_text("a b a", "a", "c", true).unwrap(), ("c b c".into(), 2));
    assert!(replace_text("a b", "x", "y", false).unwrap_err().contains("wasn't found"));
    assert!(replace_text("a b", "", "y", false).is_err());
    assert!(replace_text("a b", "a", "a", false).is_err());
    // Windows line endings still match text written with \n.
    assert_eq!(replace_text("one\r\ntwo\r\n", "one\ntwo", "1\n2", false).unwrap(), ("1\r\n2\r\n".into(), 1));
}

#[test]
fn globs() {
    assert!(glob_match("*.pdf", "Invoice.PDF"));
    assert!(glob_match("inv-??.txt", "inv-07.txt"));
    assert!(!glob_match("*.pdf", "a.pdf.txt"));
    assert!(glob_match("*report*", "2026 Report final.docx"));
    assert!(name_matches("tax", "Taxes 2026"));
}

#[tokio::test]
async fn write_asks_shows_content_and_creates() {
    let (data, cfg) = sandbox("write");
    let (r, cards) = run(&cfg, "file_write", json!({"path": "notes/todo.txt", "content": "milk\neggs\n"}), true).await;
    r.unwrap();
    assert_eq!(std::fs::read_to_string(data.join("files/notes/todo.txt")).unwrap(), "milk\neggs\n");
    assert_eq!(cards.len(), 1);
    assert!(cards[0].contains("Create") && cards[0].contains("todo.txt") && cards[0].contains("+ milk"), "{}", cards[0]);
    assert!(cards[0].contains("creating the folder"), "{}", cards[0]);

    // Existing files need overwrite: true, and the card shows a diff.
    let (r, _) = run(&cfg, "file_write", json!({"path": "notes/todo.txt", "content": "x"}), true).await;
    assert!(r.unwrap_err().contains("already exists"));
    let (r, cards) =
        run(&cfg, "file_write", json!({"path": "notes/todo.txt", "content": "milk\nbread\n", "overwrite": true}), true).await;
    r.unwrap();
    assert!(cards[0].contains("- eggs") && cards[0].contains("+ bread"), "{}", cards[0]);
    assert_eq!(std::fs::read_to_string(data.join("files/notes/todo.txt")).unwrap(), "milk\nbread\n");
}

#[tokio::test]
async fn declined_changes_leave_files_alone() {
    let (data, cfg) = sandbox("decline");
    let (r, cards) = run(&cfg, "file_write", json!({"path": "a.txt", "content": "hi"}), false).await;
    assert!(r.unwrap_err().contains("declined"));
    assert_eq!(cards.len(), 1);
    assert!(!data.join("files/a.txt").exists());
    std::fs::write(data.join("files/b.txt"), "keep").unwrap();
    let (r, _) = run(&cfg, "file_delete", json!({"path": "b.txt"}), false).await;
    assert!(r.is_err());
    assert!(data.join("files/b.txt").exists());
}

#[tokio::test]
async fn refused_paths_never_reach_the_user() {
    let (_data, cfg) = sandbox("refuse");
    for (name, input) in [
        ("file_write", json!({"path": "~/.ssh/authorized_keys", "content": "x"})),
        ("file_write", json!({"path": "../policy.json", "content": "{}"})),
        ("file_delete", json!({"path": "~"})),
        ("file_edit", json!({"path": "/etc/hosts", "old_text": "a", "new_text": "b"})),
    ] {
        let (r, cards) = run(&cfg, name, input.clone(), true).await;
        assert!(r.is_err(), "{name} {input} should fail");
        assert!(cards.is_empty(), "{name} {input} asked the user");
    }
}

#[tokio::test]
async fn new_files_in_the_workspace_files_folder_dont_ask() {
    let (data, cfg) = sandbox("free");
    let dl = data.join("workspace/Files");
    let (r, cards) = run(&cfg, "file_write", json!({"path": dl.join("summary.md").to_str().unwrap(), "content": "# Hi"}), true).await;
    assert!(r.unwrap().content.contains("don't need the user's approval"));
    assert!(cards.is_empty());
    // Replacing one does.
    let (r, cards) =
        run(&cfg, "file_write", json!({"path": dl.join("summary.md").to_str().unwrap(), "content": "# Bye", "overwrite": true}), true).await;
    r.unwrap();
    assert_eq!(cards.len(), 1);
    // The workspace's other folders ask too.
    let shots = data.join("workspace/Screenshots");
    std::fs::create_dir_all(&shots).unwrap();
    let (r, cards) = run(&cfg, "file_write", json!({"path": shots.join("a.txt").to_str().unwrap(), "content": "x"}), true).await;
    r.unwrap();
    assert_eq!(cards.len(), 1);
}

#[tokio::test]
async fn edit_shows_the_changed_lines() {
    let (data, cfg) = sandbox("edit");
    let f = data.join("files/todo.txt");
    std::fs::write(&f, "1. Call the dentist\n2. Pay the water bill\n3. Return books\n").unwrap();
    let (r, cards) =
        run(&cfg, "file_edit", json!({"path": "~/todo.txt", "old_text": "water bill", "new_text": "electric bill"}), true).await;
    r.unwrap();
    assert!(cards[0].contains("- 2. Pay the water bill") && cards[0].contains("+ 2. Pay the electric bill"), "{}", cards[0]);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "1. Call the dentist\n2. Pay the electric bill\n3. Return books\n");
    let (r, cards) = run(&cfg, "file_edit", json!({"path": "~/todo.txt", "old_text": "nope", "new_text": "x"}), true).await;
    assert!(r.is_err() && cards.is_empty());
}

#[tokio::test]
async fn delete_goes_to_the_sandbox_trash() {
    let (data, cfg) = sandbox("delete");
    std::fs::create_dir_all(data.join("files/old")).unwrap();
    std::fs::write(data.join("files/old/draft.txt"), "bye").unwrap();
    let (r, cards) = run(&cfg, "file_delete", json!({"path": "old"}), true).await;
    r.unwrap();
    assert!(cards[0].contains("Move to the Trash") && cards[0].contains("1 files"), "{}", cards[0]);
    assert!(!data.join("files/old").exists());
    assert!(data.join("Trash/files/old/draft.txt").exists());
    assert!(data.join("Trash/info/old.trashinfo").exists());
}

#[tokio::test]
async fn move_mkdir_read_list_and_search() {
    let (data, cfg) = sandbox("misc");
    let files = data.join("files");
    run(&cfg, "file_mkdir", json!({"path": "Recipes"}), true).await.0.unwrap();
    assert!(files.join("Recipes").is_dir());
    std::fs::write(files.join("pancakes.txt"), "2 eggs\nflour\na pinch of nutmeg\n").unwrap();
    std::fs::write(files.join(".hidden"), "nope").unwrap();
    let (r, cards) = run(&cfg, "file_move", json!({"from": "pancakes.txt", "to": "Recipes"}), true).await;
    r.unwrap();
    assert!(cards[0].contains("→"), "{}", cards[0]);
    assert!(files.join("Recipes/pancakes.txt").exists());
    let (r, _) = run(&cfg, "file_move", json!({"from": "Recipes", "to": "Recipes/inner"}), true).await;
    assert!(r.is_err());

    let read = tool(&cfg, "file_read").call(json!({"path": "Recipes/pancakes.txt", "offset": 2, "limit": 1})).await.unwrap();
    assert!(read.content.contains("     2\tflour") && !read.content.contains("eggs"), "{}", read.content);
    assert!(read.content.contains("Pass offset: 3"), "{}", read.content);

    let list = tool(&cfg, "file_list").call(json!({})).await.unwrap();
    assert!(list.content.contains("Recipes/") && !list.content.contains(".hidden"), "{}", list.content);
    assert!(list.content.contains("1 hidden or private"), "{}", list.content);

    let found = tool(&cfg, "file_search").call(json!({"contains": "NUTMEG"})).await.unwrap();
    assert!(found.content.contains("Recipes/pancakes.txt:3: a pinch of nutmeg"), "{}", found.content);
    let found = tool(&cfg, "file_search").call(json!({"name": "*.txt"})).await.unwrap();
    assert!(found.content.contains("pancakes.txt"), "{}", found.content);
}

#[tokio::test]
async fn replacing_a_big_file_doesnt_read_it() {
    let (data, cfg) = sandbox("bigwrite");
    let f = data.join("files/huge.log");
    // Sparse: big by its size, nothing on disk.
    std::fs::File::create(&f).unwrap().set_len(MAX_TEXT + 1024 * 1024).unwrap();
    let (r, cards) = run(&cfg, "file_write", json!({"path": "huge.log", "content": "short now\n", "overwrite": true}), true).await;
    r.unwrap();
    assert!(cards[0].contains("replaces a 6.0 MB file") && !cards[0].contains("- "), "{}", cards[0]);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "short now\n");
}

#[tokio::test]
async fn a_long_first_line_is_cut() {
    let (data, cfg) = sandbox("longline");
    std::fs::write(data.join("files/min.js"), format!("{}\nsecond\n", "é".repeat(100_000))).unwrap();
    let read = tool(&cfg, "file_read").call(json!({"path": "min.js", "max_chars": 2000})).await.unwrap();
    assert!(read.content.len() <= 2000, "{} bytes", read.content.len());
    assert!(read.content.contains("line 1 was cut"), "{}", read.content);
    assert!(read.content.contains("Pass offset: 2"), "{}", read.content);
    // Later lines are left for the next read.
    let read = tool(&cfg, "file_read").call(json!({"path": "min.js", "offset": 2})).await.unwrap();
    assert!(read.content.contains("     2\tsecond"), "{}", read.content);
}

#[tokio::test]
async fn pipes_and_sockets_arent_read() {
    let (data, cfg) = sandbox("special");
    let _sock = std::os::unix::net::UnixListener::bind(data.join("files/app.sock")).unwrap();
    let r = tool(&cfg, "file_read").call(json!({"path": "app.sock"})).await;
    assert!(r.unwrap_err().contains("isn't a regular file"));
    let (r, cards) = run(&cfg, "file_edit", json!({"path": "app.sock", "old_text": "a", "new_text": "b"}), true).await;
    assert!(r.unwrap_err().contains("isn't a regular file") && cards.is_empty());
    let (r, cards) = run(&cfg, "file_write", json!({"path": "app.sock", "content": "x", "overwrite": true}), true).await;
    assert!(r.unwrap_err().contains("isn't a regular file") && cards.is_empty());
}

#[test]
fn moving_across_drives_never_leaves_a_partial_file() {
    let (data, _) = sandbox("across");
    let dir = data.join("files");
    let (from, to) = (dir.join("a.txt"), dir.join("b.txt"));
    std::fs::write(&from, "hello").unwrap();
    move_across(&from, &to).unwrap();
    assert!(!from.exists());
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "hello");

    // Something appeared at the destination: it stays, and so does the original.
    std::fs::write(&from, "new").unwrap();
    assert!(move_across(&from, &to).unwrap_err().contains("appeared"));
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "hello");
    assert_eq!(std::fs::read_to_string(&from).unwrap(), "new");

    // A failed copy leaves nothing behind.
    assert!(move_across(&dir.join("missing.txt"), &dir.join("c.txt")).is_err());
    let mut left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    left.sort();
    assert_eq!(left, ["a.txt", "b.txt"]);
}

#[tokio::test]
async fn long_file_names_can_be_replaced() {
    let (data, cfg) = sandbox("longname");
    // 250 bytes: room for the name, but not for one built from it.
    let name = format!("{}.txt", "n".repeat(246));
    std::fs::write(data.join("files").join(&name), "old\n").unwrap();
    let (r, _) = run(&cfg, "file_write", json!({"path": name.clone(), "content": "new\n", "overwrite": true}), true).await;
    r.unwrap();
    let (r, _) = run(&cfg, "file_edit", json!({"path": name.clone(), "old_text": "new", "new_text": "newer"}), true).await;
    r.unwrap();
    assert_eq!(std::fs::read_to_string(data.join("files").join(&name)).unwrap(), "newer\n");
    assert_eq!(std::fs::read_dir(data.join("files")).unwrap().count(), 1, "a temporary file was left behind");
}
