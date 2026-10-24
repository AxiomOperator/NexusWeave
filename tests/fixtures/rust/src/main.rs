mod models;
mod repo;

use models::Named;
use repo::Repo;

fn main() {
    let r = Repo::new();
    let u = r.find("1").unwrap();
    u.greet();
    let s = crate::models::User::new("x");
    s.name();
    helper();
}

fn helper() {}

fn describe<T: Named>(t: &T) -> String { t.name() }
