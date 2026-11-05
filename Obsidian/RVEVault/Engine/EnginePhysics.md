# Engine Physics

`engine/engine-physics` содержит CPU-геометрию столкновений и запросы без оконного или GPU-пути. `Shape` поддерживает sphere, box, capsule, convex hull и triangle mesh; формы используют локальные координаты и rigid `Pose` без масштаба. Выпуклые формы предоставляют support mapping, AABB, объём, центр массы и тензор инерции; triangle mesh не имеет динамических mass properties. Quickhull принимает до 4096 входных точек, ограничен 256 вершинами и 256 итерациями. BVH ограничивает mesh одним миллионом вершин и 262144 треугольниками.

`broad::sweep_and_prune` строит детерминированные пары пересекающихся AABB по оси X. Число элементов ограничено 16384, проверок и пар — одним миллионом. `narrow::collide` возвращает контактный manifold до четырёх точек для выпуклых пар и пары convex–triangle mesh; mesh-кандидаты запрашиваются из BVH и проверяются по фактическим треугольникам. Выпуклые запросы используют bounded GJK/EPA; `narrow::distance` возвращает расстояние и мировые witness points. `ManifoldCache` хранит локальные anchors и накопленные импульсы по caller-provided `PairKey`; жизненный цикл пар принадлежит вызывающей симуляции.

GJK ограничен 64 итерациями, EPA — 64 итерациями и 128 гранями. Broadphase и число mesh-кандидатов также ограничены; превышение возвращается как ошибка. `ray::raycast` проверяет sphere, box, capsule, hull planes и BVH triangles в заданной позе. Узкие пары sphere–sphere, sphere–box, sphere–capsule и capsule–capsule используют аналитические контакты; box–box использует SAT. Hull и прочие выпуклые пары проходят support-mapped narrowphase.

`PhysicsEngine::run` пока остаётся совместимой потоковой заглушкой и ждёт сигнал остановки. В этом контракте ещё нет `PhysicsWorld`, solver, событий, команд или runtime-интеграции.

Источник: `engine/engine-physics/src/{broad.rs,narrow.rs,shape.rs,hull.rs,bvh.rs,ray.rs}`.
