package com.app;

import com.app.repo.Repo;
import java.util.ArrayList;
import java.util.List;

public class Service {
    private final Repo repo = new Repo();

    public <T extends com.app.models.Person> String hello(T p) { return p.name(); }

    public void handle() {
        var u = repo.find("1");
        u.greet();
        u.name();
        Helper.util();
        List<String> l = new ArrayList<>();
        l.add("x");
    }
}
