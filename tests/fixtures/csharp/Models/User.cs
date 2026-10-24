namespace App.Models
{
    public interface IEntity { int Id { get; } }

    public class User : IEntity
    {
        public int Id { get; set; }
        public string Greet() { return "hi"; }
    }
}
