import { MemRepo } from "./repo";
import { BaseService } from "./base";
import * as models from "./models";
import { Db } from "./models";
import express from "express";

export class UserService extends BaseService {
  private repo = new MemRepo();
  constructor(private db: Db) { super(); }
  getUser(id: string) {
    const u = this.repo.find(id);
    this.log("got");
    u.greet();
    this.db.query("select");
    return new models.User("x");
  }
}

export function makeService(): UserService { return new UserService(new Db()); }

export function main() {
  const s = makeService();
  s.getUser("1");
  const app = express();
  app.get("/");
  const handler = () => s.getUser("2");
  handler();
}
