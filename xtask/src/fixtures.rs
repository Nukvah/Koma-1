//! `cargo xtask fixtures <dir>`: writes the made-up test library.

use std::path::Path;

pub fn run(args: &[String]) -> Result<(), String> {
    let [dir] = args else {
        return Err("usage: cargo xtask fixtures <dir>".into());
    };
    let root = Path::new(dir);
    let books = koma_fixtures::library::write(root).map_err(|e| format!("{dir}: {e}"))?;
    for book in &books {
        println!("{:>3} pages  {}", book.pages.len(), book.path);
    }
    println!("wrote {} made-up books to {}", books.len(), root.display());
    Ok(())
}
