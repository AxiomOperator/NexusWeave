import express from "express";
import users from "./users";

const app = express();
app.use("/api/users", users);
