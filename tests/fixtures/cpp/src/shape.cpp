#include "../include/shape.h"
namespace geo {
void Shape::describe() { area(); }
Circle::Circle(double r) : r_(r) {}
double Circle::area() const { return 3.14 * r_ * r_; }
}  // namespace geo
