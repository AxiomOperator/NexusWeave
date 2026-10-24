import { User } from "@/models";
import { MemRepo } from "./repo";

export class Container {
  get<T>(key: string): T { return null as any; }
}

export function createUser() {
  return new User("x");
}

export const makeRepo = () => new MemRepo();

export function wire(c: Container) {
  createUser().greet();
  const r = makeRepo();
  r.find("1");
  const repo = c.get<MemRepo>("repo");
  repo.find("2");
}
