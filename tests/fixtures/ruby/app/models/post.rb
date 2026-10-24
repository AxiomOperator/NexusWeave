class ApplicationRecord < ActiveRecord::Base
end

class Post < ApplicationRecord
  def shout
    title.upcase
  end
end
