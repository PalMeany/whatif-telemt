import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { ShieldCheck, TriangleAlert } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Notice } from "@/components/ui/feedback";
import { PageSection, QueryState, RefreshButton, SectionCard } from "@/components/page";
import { panelApi } from "@/lib/api";
import { formatRelative, formatTime } from "@/lib/utils";
import type { AuditRecord } from "@/lib/types";

type Verification = { checked: number; valid: boolean; broken_at: number | null };

export default function AuditPage() {
  const [limit, setLimit] = React.useState(200);
  const [filter, setFilter] = React.useState("");
  const [verification, setVerification] = React.useState<Verification | null>(null);
  const [verifying, setVerifying] = React.useState(false);

  const audit = useQuery({
    queryKey: ["audit", limit],
    queryFn: () =>
      panelApi<{ records: AuditRecord[]; enabled: boolean }>(`/audit?limit=${limit}`),
    retry: false,
  });

  const rows = React.useMemo(() => {
    const list = audit.data?.records ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return list;
    return list.filter((record) =>
      `${record.actor} ${record.action} ${record.target} ${record.node} ${record.result} ${record.detail}`
        .toLowerCase()
        .includes(needle),
    );
  }, [audit.data, filter]);

  async function verify() {
    setVerifying(true);
    try {
      setVerification(await panelApi<Verification>("/audit/verify"));
    } finally {
      setVerifying(false);
    }
  }

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Every mutating panel action, hash-chained so an edit anywhere invalidates everything after it."
        actions={
          <>
            <Input
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter records"
              className="h-9 w-52"
              aria-label="Filter audit records"
            />
            <Input
              value={limit}
              onChange={(event) => setLimit(Math.min(1000, Number(event.target.value) || 200))}
              inputMode="numeric"
              className="h-9 w-20 tabular"
              aria-label="Record limit"
            />
            <Button variant="outline" size="sm" onClick={() => void verify()} disabled={verifying}>
              <ShieldCheck />
              {verifying ? "Verifying…" : "Verify chain"}
            </Button>
            <RefreshButton onClick={() => void audit.refetch()} busy={audit.isFetching} />
          </>
        }
      >
        {audit.data && !audit.data.enabled ? (
          <Notice tone="warn" title="Auditing is disabled">
            Set <code>panel.audit_enabled = true</code> to record actions.
          </Notice>
        ) : null}
        {verification ? (
          verification.valid ? (
            <Notice title="Chain verified">
              {verification.checked} records verified end to end.
            </Notice>
          ) : (
            <Notice tone="danger" title="Chain is broken">
              <span className="inline-flex items-center gap-1.5">
                <TriangleAlert className="size-3.5" />
                Verification failed at record {verification.broken_at ?? "?"} after{" "}
                {verification.checked} records.
              </span>
            </Notice>
          )
        ) : null}

        <SectionCard bodyClassName="px-0 pb-0">
          <QueryState
            isLoading={audit.isLoading}
            error={audit.error}
            isEmpty={rows.length === 0}
            emptyTitle={filter ? "No record matches that filter" : "No audit records yet"}
            skeletonRows={8}
          >
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-16 text-right">Seq</TableHead>
                  <TableHead className="w-44">Time</TableHead>
                  <TableHead>Actor</TableHead>
                  <TableHead>Action</TableHead>
                  <TableHead>Target</TableHead>
                  <TableHead>Node</TableHead>
                  <TableHead>Result</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((record) => (
                  <TableRow key={`${record.seq}-${record.hash}`}>
                    <TableCell className="tabular text-right text-muted-foreground">
                      {record.seq}
                    </TableCell>
                    <TableCell className="tabular whitespace-nowrap text-[12px]">
                      <div>{formatTime(record.ts)}</div>
                      <div className="text-muted-foreground">{formatRelative(record.ts)}</div>
                    </TableCell>
                    <TableCell>
                      <div className="font-medium">{record.actor || "—"}</div>
                      {record.address ? (
                        <div className="font-mono text-[11px] text-muted-foreground">
                          {record.address}
                        </div>
                      ) : null}
                    </TableCell>
                    <TableCell className="font-mono text-[12px]">{record.action}</TableCell>
                    <TableCell className="max-w-[18rem] truncate font-mono text-[12px]" title={record.target}>
                      {record.target || "—"}
                      {record.detail ? (
                        <div className="truncate text-[11px] text-muted-foreground" title={record.detail}>
                          {record.detail}
                        </div>
                      ) : null}
                    </TableCell>
                    <TableCell className="font-mono text-[12px] text-muted-foreground">
                      {record.node || "—"}
                    </TableCell>
                    <TableCell>
                      <Badge variant={resultTone(record.result)}>{record.result}</Badge>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </QueryState>
        </SectionCard>
      </PageSection>
    </div>
  );
}

function resultTone(result: string): "ok" | "warn" | "danger" | "default" {
  if (result === "ok" || result.startsWith("2")) return "ok";
  if (result.startsWith("4")) return "warn";
  if (result.startsWith("5") || result.includes("fail") || result.includes("bad")) return "danger";
  return "default";
}
