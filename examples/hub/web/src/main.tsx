import { App as AntApp, ConfigProvider } from "antd";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import "highlight.js/styles/github.css";
import "./style.css";
import { Shell } from "./Shell";
import { SessionProvider } from "./session";
import { Home } from "./pages/Home";
import { Login } from "./pages/Login";
import { NewOrg, NewRepo } from "./pages/New";
import { OwnerPage } from "./pages/Owner";
import { Keys } from "./pages/Keys";
import { Tokens } from "./pages/Tokens";
import { Repo } from "./pages/Repo";
import { TeamPage } from "./pages/Team";

const theme = {
  token: { colorPrimary: "#1f6f5c", borderRadius: 6, fontFamily: "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif" },
};

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ConfigProvider theme={theme}>
      <AntApp>
        <BrowserRouter>
          <SessionProvider>
            <Routes>
              <Route element={<Shell />}>
                <Route index element={<Home />} />
                <Route path="login" element={<Login mode="login" />} />
                <Route path="signup" element={<Login mode="signup" />} />
                <Route path="new" element={<NewRepo />} />
                <Route path="orgs/new" element={<NewOrg />} />
                <Route path="orgs/:org/teams/:team" element={<TeamPage />} />
                <Route path="settings/tokens" element={<Tokens />} />
                <Route path="settings/keys" element={<Keys />} />
                <Route path=":owner" element={<OwnerPage />} />
                <Route path=":owner/:repo/*" element={<Repo />} />
              </Route>
            </Routes>
          </SessionProvider>
        </BrowserRouter>
      </AntApp>
    </ConfigProvider>
  </StrictMode>,
);
