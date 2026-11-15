package io.github.ajmalx0.orbit.examples;

import io.github.ajmalx0.orbit.TaskException;
import io.github.ajmalx0.orbit.Worker;

/** Java part of examples/java_hello.orbit: greeting.java. */
public final class HelloWorker {
    record Input(String name, boolean excited) {}

    record Output(String message, int letters) {}

    public static void main(String[] args) {
        Worker worker = Worker.builder().workerId("hello-java").build();
        worker.task("greeting.java", "1.0.0", Input.class, (in, ctx) -> {
            String name = in.name() == null ? "" : in.name().strip();
            if (name.isEmpty()) {
                throw TaskException.permanent("INVALID_NAME", "name must not be blank");
            }
            int letters = (int) name.codePoints().filter(Character::isLetter).count();
            String message = "Hello, " + name + ", from Java" + (in.excited() ? "!" : ".");
            return new Output(message, letters);
        });
        // SIGINT/SIGTERM: finish in-flight tasks, then unregister.
        Runtime.getRuntime().addShutdownHook(new Thread(worker::stop));
        worker.run();
    }
}
