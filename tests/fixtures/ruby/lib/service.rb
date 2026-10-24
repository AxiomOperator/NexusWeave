require_relative "models/user"

module App
  class Service
    def initialize
      @repo = Repo.new
    end

    def run
      u = User.new
      u.greet
      u.save
      @repo.find
      helper
    end

    def helper
    end
  end

  class Repo
    def find
    end
  end
end
