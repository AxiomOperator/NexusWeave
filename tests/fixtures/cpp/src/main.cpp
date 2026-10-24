#include <cstdio>
#include "../include/shape.h"
static void helper() {}
int main() {
  auto* s = new geo::Circle(1.0);
  s->area();
  s->describe();
  helper();
  printf("done");
  return 0;
}
