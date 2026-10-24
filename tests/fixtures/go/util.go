package main

func util() { helper() }

func run(f func(*Server)) { f(&Server{}) }
