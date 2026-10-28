/* A stand-in program for the demo only: a process whose name is what tmux names the
 * process in front of a pane. taw's watcher goes by that name, and a stand-in agent that
 * is a shell script is named bash. Built once, then copied under the name wanted:
 *
 *   docker    taw reads the arguments of a container runtime's command line to find the
 *             agent a container runs, so it is started as `docker run ... image claude`
 *   opencode  OpenCode's idle state comes only from its hook, and taw takes the hook's word
 *             for a pane only while the program in front of it is called opencode
 *
 * It ignores every argument and runs the stand-in agent that TAW_DEMO_STANDIN holds as a
 * shell command line, passing the signals that move the agent along on to it. */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child;

static void forward(int sig) {
  if (child > 0) kill(child, sig);
}

int main(void) {
  const char *line = getenv("TAW_DEMO_STANDIN");
  if (!line) return 2;
  int sigs[] = {SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGUSR2};
  for (unsigned i = 0; i < sizeof sigs / sizeof *sigs; i++) signal(sigs[i], forward);
  child = fork();
  if (child < 0) {
    perror("standin: fork");
    return 1;
  }
  if (child == 0) {
    for (unsigned i = 0; i < sizeof sigs / sizeof *sigs; i++) signal(sigs[i], SIG_DFL);
    execl("/bin/bash", "bash", "-c", line, (char *)0);
    _exit(127);
  }
  int status = 0;
  while (waitpid(child, &status, 0) < 0) {
    if (errno != EINTR) return 1;
  }
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
