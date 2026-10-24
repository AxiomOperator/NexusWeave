#include <memory>
#include "../include/shape.h"
void run() {
  std::unique_ptr<geo::Circle> c(new geo::Circle(1.0));
  c->area();
  auto p = std::make_unique<geo::Circle>(2.0);
  p->describe();
}
