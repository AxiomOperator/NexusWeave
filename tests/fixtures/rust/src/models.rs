pub struct User {
    pub name: String,
}

impl User {
    pub fn new(n: &str) -> Self { User { name: n.to_string() } }
    pub fn greet(&self) -> String { format!("hi {}", self.name) }
}

pub trait Named {
    fn name(&self) -> String;
}

impl Named for User {
    fn name(&self) -> String { self.name.clone() }
}
