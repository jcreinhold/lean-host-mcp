// Darwin process observations for the disposable warm-worker regression.
#include <errno.h>
#include <libproc.h>
#include <stdint.h>
#include <sys/proc_info.h>

int stdin_pipe(pid_t pid, int *status, int64_t *bytes) {
  struct pipe_fdinfo info;
  int size = proc_pidfdinfo(pid, 0, PROC_PIDFDPIPEINFO, &info, sizeof(info));
  if (size != sizeof(info))
    return size == 0 && errno ? errno : EIO;
  *status = info.pipeinfo.pipe_status;
  *bytes = info.pipeinfo.pipe_stat.vst_size;
  return 0;
}

int birth(pid_t pid, uint64_t *seconds, uint64_t *micros) {
  struct proc_bsdinfo info;
  int size = proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info));
  if (size != sizeof(info))
    return size == 0 && errno ? errno : ESRCH;
  *seconds = info.pbi_start_tvsec;
  *micros = info.pbi_start_tvusec;
  return 0;
}
