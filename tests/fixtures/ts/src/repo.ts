import { User, Repo } from "@/models";
export class MemRepo implements Repo {
  find(id: string): User { return new User(id); }
}
