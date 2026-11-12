//! Evaluates an expression and prints every derivation it instantiated,
//! as ATerm, the way they would be written to the store.
//!
//! ```text
//! cargo run -p brix-eval --example show_derivations -- 'derivation { name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; }'
//! ```

use brix_eval::{Arena, Config, Evaluator, NullHost};
use brix_store::{StoreDir, StoreObject};

fn main() {
    let expr = std::env::args()
        .nth(1)
        .expect("usage: show_derivations <expression>");
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    match ev.eval_str(expr.as_bytes(), "/") {
        Ok(v) => {
            let cell = ev.alloc_cell(v);
            if let Err(e) = ev.force_deep(cell) {
                eprintln!("{}", e.render(true));
                std::process::exit(1);
            }
            println!("{}", ev.print_ambiguous(cell).expect("printable"));
        }
        Err(e) => {
            eprintln!("{}", e.render(true));
            std::process::exit(1);
        }
    }
    let store_dir = StoreDir::default();
    for (path, object) in ev.virtual_store().iter() {
        if let StoreObject::Derivation(drv) = object {
            println!(
                "\n{}:\n{}",
                store_dir.display(path),
                bstr::BStr::new(&drv.to_aterm(&store_dir).expect("serialisable"))
            );
        }
    }
}
