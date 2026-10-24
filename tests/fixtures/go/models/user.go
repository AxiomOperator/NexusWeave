package models

type User struct{ Name string }

func NewUser(n string) *User { return &User{Name: n} }

func (u *User) Greet() string { return "hi " + u.Name }
