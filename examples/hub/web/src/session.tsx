import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import { api, type Me } from "./api";

type Session = { me: Me | undefined; refresh: () => Promise<void> };
const Context = createContext<Session>({ me: undefined, refresh: async () => {} });
const NOBODY: Me = { user: null, orgs: [], signIn: { passwords: true, providers: [] } };

/** Who is signed in. `me` is undefined until the first answer. */
export function SessionProvider({ children }: { children: ReactNode }) {
  const [me, setMe] = useState<Me>();
  const refresh = useCallback(async () => setMe(await api<Me>("/me").catch(() => NOBODY)), []);
  useEffect(() => void refresh(), [refresh]);
  return <Context.Provider value={{ me, refresh }}>{children}</Context.Provider>;
}

export const useSession = () => useContext(Context);
