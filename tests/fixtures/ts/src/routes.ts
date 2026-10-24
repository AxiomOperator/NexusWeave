import express from "express";
import { UserService } from "./service";

const app = express();

function auth(req: any, res: any, next: any) { next(); }

app.get("/users/:id", auth, (req: any, res: any) => {
  new UserService(null as any).getUser(req.params.id);
});

function tick() {}
function double(x: number) { return x * 2; }

export function schedule() {
  setTimeout(tick, 10);
  [1, 2].map(double);
}

@Controller("users")
export class UsersController {
  @Get(":id")
  find(id: string) { return id; }
}
