class PostsController
  def show
    Post.where(published: true).first.shout
    Post.find(1).shout
  end
end
