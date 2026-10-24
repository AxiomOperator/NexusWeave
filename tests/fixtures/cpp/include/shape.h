#pragma once
namespace geo {
class Shape {
 public:
  virtual double area() const = 0;
  void describe();
};
class Circle : public Shape {
 public:
  explicit Circle(double r);
  double area() const override;
 private:
  double r_;
};
}  // namespace geo
