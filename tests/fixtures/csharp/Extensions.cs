using App.Models;
using Microsoft.Extensions.DependencyInjection;

namespace App
{
    public static class Extensions
    {
        public static string Shout(this User u) { return u.Greet().ToUpper(); }
        public static IServiceCollection AddApp(this IServiceCollection services) { return services; }
    }

    public class Startup
    {
        public void Configure(IServiceCollection services, User user)
        {
            services.AddApp();
            user.Shout();
        }
    }
}
