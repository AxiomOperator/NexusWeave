import { Routes, Route, Link, useNavigate } from "react-router-dom";
import { UserPage, UsersList } from "./pages/UserPage";
import { Settings } from "./Settings";

export function App() {
  return (
    <Routes>
      <Route path="/users" element={<UsersList />}>
        <Route path=":id" element={<UserPage />} />
      </Route>
      <Route path="/settings" Component={Settings} />
    </Routes>
  );
}

export function Header() {
  return <Link to="/settings">Settings</Link>;
}

export function useOpenUser() {
  const navigate = useNavigate();
  return function openUser(id: string) {
    navigate(`/users/${id}`);
  };
}

export function goHome() {
  navigate("/nowhere/at/all");
}
