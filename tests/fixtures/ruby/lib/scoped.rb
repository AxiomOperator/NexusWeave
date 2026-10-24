# Fully qualified constants: each segment of `App::User` refers to a module or class.
class Scoped
  def build
    App::User.new
  end
end
