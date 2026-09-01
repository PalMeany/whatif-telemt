import * as React from "react";
import { panelApi } from "@/lib/api";

type Theme = "dark" | "light";

const STORAGE_KEY = "telemt.panel.theme";

/**
 * Theme preference.
 *
 * Kept in local storage so the first paint after a reload is already correct,
 * and mirrored to the panel store so the same operator gets the same theme on
 * another machine.
 */
export function useTheme() {
  const [theme, setThemeState] = React.useState<Theme>(() => {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    return stored === "light" ? "light" : "dark";
  });

  React.useEffect(() => {
    document.documentElement.classList.toggle("dark", theme === "dark");
    document.documentElement.style.colorScheme = theme;
  }, [theme]);

  const setTheme = React.useCallback((next: Theme) => {
    window.localStorage.setItem(STORAGE_KEY, next);
    setThemeState(next);
    void panelApi("/settings", { method: "PATCH", body: { appearance: next } }).catch(() => {
      // A preference that fails to persist is not worth interrupting a session
      // for; the local value already took effect.
    });
  }, []);

  return { theme, setTheme };
}
