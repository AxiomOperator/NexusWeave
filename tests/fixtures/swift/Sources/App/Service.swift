func helper(_ x: Int) -> Int { return x + 1 }

class Service {
    let repo: Repo
    init(repo: Repo) { self.repo = repo }

    func handle(id: Int) -> String {
        let user = repo.find(id: helper(id))
        return user.greet() + user.shout()
    }

    func direct(u: User) -> String {
        return u.display()
    }
}
