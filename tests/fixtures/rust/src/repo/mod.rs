use crate::models::User;

pub struct Repo {
    users: Vec<User>,
}

impl Repo {
    pub fn new() -> Repo { Repo { users: Vec::new() } }
    pub fn find(&self, id: &str) -> Option<User> { Some(User::new(id)) }
}
