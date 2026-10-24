using System;
using App.Models;

namespace App.Services
{
    public interface IRepo { User Find(int id); }

    public class UserService
    {
        private readonly IRepo _repo;
        public UserService(IRepo repo) { _repo = repo; }

        public User Get(int id)
        {
            var u = _repo.Find(id);
            u.Greet();
            Console.WriteLine("x");
            return u;
        }
    }
}
