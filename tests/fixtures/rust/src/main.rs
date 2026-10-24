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

fn pick(n: u32) -> u32 { n }

fn with_closure() -> u32 {
    let pick = |n: u32| n + 1;
    pick(2)
}

fn with_param(helper: impl Fn()) { helper() }

fn shadow_after() -> u32 {
    let pick = pick(1);
    pick
}

extern "C" fn on_signal(_s: i32) {}

fn install() -> usize { on_signal as usize }
