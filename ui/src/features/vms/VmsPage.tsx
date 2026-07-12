import { RefreshCw, Square, Play, Pause } from 'lucide-react';
import { useVmList, useVmMutations } from '../../hooks/queries/useVmList';
import type { Vm } from '../../hooks/queries/useVmList';
import { Card, StatusBadge, EmptyState, Skeleton } from '../../components/ui';
import { useVmStats } from '../../hooks/queries/useVmStats';

export function VmsPage() {
  const { data, isLoading, refetch } = useVmList();

  return (
    <div className="p-8 max-w-6xl mx-auto">
      <header className="flex items-center justify-between mb-8">
        <div>
          <h2 className="text-2xl font-bold text-foreground mb-1">MicroVMs</h2>
          <p className="text-sm text-muted-foreground">Manage your running micro-virtual machines.</p>
        </div>
        <button
          onClick={() => refetch()}
          className="p-2 bg-card border border-border rounded hover:bg-muted transition text-muted-foreground hover:text-foreground"
        >
          <RefreshCw size={18} />
        </button>
      </header>

      <Card>
        <div className="grid grid-cols-12 gap-4 p-4 border-b border-border text-xs font-semibold text-muted-foreground uppercase tracking-wider bg-card">
          <div className="col-span-1 flex justify-center">Status</div>
          <div className="col-span-3">Name / ID</div>
          <div className="col-span-2">Image</div>
          <div className="col-span-2">IP Address</div>
          <div className="col-span-3">Usage</div>
          <div className="col-span-1 text-right">Actions</div>
        </div>
        <div className="divide-y divide-border/50 bg-card">
          {isLoading ? (
            Array.from({ length: 3 }).map((_, i) => (
              <div key={i} className="grid grid-cols-12 gap-4 p-4 items-center">
                <div className="col-span-1 flex justify-center"><Skeleton className="h-6 w-16 rounded-full" /></div>
                <div className="col-span-3 space-y-2">
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-3 w-24" />
                </div>
                <div className="col-span-2"><Skeleton className="h-4 w-20" /></div>
                <div className="col-span-2"><Skeleton className="h-4 w-24" /></div>
                <div className="col-span-3"><Skeleton className="h-4 w-32" /></div>
                <div className="col-span-1 flex justify-end gap-2">
                  <Skeleton className="h-8 w-8 rounded" />
                </div>
              </div>
            ))
          ) : !data?.vms?.length ? (
            <EmptyState title="No MicroVMs running" />
          ) : (
            data.vms.map((vm) => (
              <VmRow key={vm.id} vm={vm} />
            ))
          )}
        </div>
      </Card>
    </div>
  );
}

export function VmRow({ vm }: { vm: Vm }) {
  const { startVm, stopVm, pauseVm, resumeVm } = useVmMutations();
  const isRunning = vm.status?.startsWith('Running');
  const isPaused = vm.status?.startsWith('Paused');

  const { data: stats } = useVmStats(vm.id, isRunning);

  const formatBytes = (bytes: number) => {
    if (!bytes) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

  return (
    <div className="grid grid-cols-12 gap-4 p-4 items-center group hover:bg-muted/30 transition">
      <div className="col-span-1 flex justify-center">
        <StatusBadge 
          status={
            vm.status?.startsWith('Running') ? 'running' :
            vm.status?.startsWith('AttestationFailed') ? 'attestation_failed' :
            vm.status?.startsWith('PendingAttestation') ? 'pending_attestation' :
            vm.status?.startsWith('Error') ? 'error' :
            vm.status?.startsWith('Paused') ? 'paused' : 'stopped'
          } 
        />
      </div>
      <div className="col-span-3">
        <div className="font-semibold text-foreground text-sm">
          {vm.labels['vyoma.service'] || 'MicroVM'}
        </div>
        <div className="text-xs text-muted-foreground font-mono mt-0.5 truncate" title={vm.id}>
          {vm.id.substring(0, 12)}
        </div>
      </div>
      <div className="col-span-2 text-sm text-muted-foreground">
        {vm.labels['image'] || 'alpine'}
      </div>
      <div className="col-span-2 text-sm text-muted-foreground font-mono">
        {vm.ip_address || '--'}
      </div>
      <div className="col-span-3 flex flex-col gap-1 justify-center text-xs text-muted-foreground">
        {isRunning && stats ? (
          <>
            <div><span className="font-medium text-foreground">{stats.cpu_percent.toFixed(1)}%</span> CPU</div>
            <div><span className="font-medium text-foreground">{formatBytes(stats.memory_usage_bytes)}</span> RAM</div>
          </>
        ) : (
          <span className="opacity-50">--</span>
        )}
      </div>
      <div className="col-span-1 flex justify-end gap-2 opacity-60 group-hover:opacity-100 transition">
        {isRunning ? (
          <button 
            onClick={() => pauseVm.mutate(vm.id)}
            className="p-1.5 hover:bg-muted rounded text-muted-foreground hover:text-yellow-400"
            title="Pause"
          >
            <Pause size={16} />
          </button>
        ) : isPaused ? (
          <button 
            onClick={() => resumeVm.mutate(vm.id)}
            className="p-1.5 hover:bg-muted rounded text-muted-foreground hover:text-green-400"
            title="Resume"
          >
            <Play size={16} />
          </button>
        ) : (
          <button 
            onClick={() => startVm.mutate(vm.id)}
            className="p-1.5 hover:bg-muted rounded text-muted-foreground hover:text-green-400"
            title="Start"
          >
            <Play size={16} />
          </button>
        )}
        
        <button 
          onClick={() => stopVm.mutate(vm.id)}
          className="p-1.5 hover:bg-destructive/20 rounded text-muted-foreground hover:text-destructive"
          title="Stop"
        >
          <Square size={16} />
        </button>
      </div>
    </div>
  );
}
