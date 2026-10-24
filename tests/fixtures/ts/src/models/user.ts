export class User {
  constructor(public name: string) {}
  greet(): string { return "hi " + this.name; }
}
export interface Repo { find(id: string): User; }
