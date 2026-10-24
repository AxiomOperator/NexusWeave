package main

import (
	"fmt"

	"example.com/app/repo"
)

type Server struct {
	repo *repo.Repo
}

func (s *Server) Handle(id string) {
	u := s.repo.Find(id)
	u.Greet()
	fmt.Println(u)
	helper()
}

func helper() {}

func main() {
	s := &Server{}
	s.Handle("1")
	run(func(srv *Server) { srv.Handle("2") })
}

type Router struct{}

func (r *Router) GET(path string, h func()) {}

func handleUser() { helper() }

func routes(r *Router) {
	r.GET("/users/:id", handleUser)
}

type Greeter interface{ Greet() string }

func greetAll[T Greeter](t T) { t.Greet() }
