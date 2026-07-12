import { useQuery } from '@tanstack/react-query';
import { api } from '../../lib/api-client';

export interface VmStats {
  vm_id: string;
  cpu_percent: number;
  memory_usage_bytes: number;
  memory_limit_bytes: number;
  memory_percent: number;
}

export function useVmStats(vmId: string, enabled: boolean = true) {
  return useQuery({
    queryKey: ['vm_stats', vmId],
    queryFn: () => api.get<VmStats>(`/vms/${vmId}/stats`),
    enabled: enabled && !!vmId,
    refetchInterval: 2000, // Poll every 2 seconds
    retry: false, // Don't keep retrying on fail (e.g. if VM stops)
  });
}
