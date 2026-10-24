using App.Models;
using App.Services;

namespace App.Controllers
{
    [Route("api/[controller]")]
    public class UsersController
    {
        private readonly IRepo _repo;
        [HttpGet("{id}")]
        public User Get(int id) { return _repo.Find(id); }
    }

    public class SqlRepo : IRepo
    {
        public User Find(int id) { return new User(); }
    }
}
