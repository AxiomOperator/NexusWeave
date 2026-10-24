package repo

import "example.com/app/models"

type Repo struct {
	cache map[string]*models.User
}

func (r *Repo) Find(id string) *models.User { return models.NewUser(id) }
