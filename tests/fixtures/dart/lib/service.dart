import 'package:app/models/user.dart';
import 'repo/repo.dart';

int helper(int x) {
  return x + 1;
}

class Service {
  final Repo repo;
  Service(this.repo);

  String handle(int id) {
    final user = repo.find(helper(id));
    return user.greet();
  }

  String direct(User u) {
    return u.display();
  }
}
